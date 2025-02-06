use alloc::boxed::Box;
use core::future::Future;

use async_recursion::async_recursion;
use byteorder::{ByteOrder, NativeEndian};
use cslice::CMutSlice;
use ksupport::rpc::{tag::{Tag, TagIterator},
                    *};
use libasync::smoltcp::TcpStream;
use libboard_zynq::smoltcp;
use log::trace;

use crate::proto_async;

/// Reads (deserializes) `length` array or list elements of type `tag` from `stream`,
/// writing them into the buffer given by `storage`.
///
/// `alloc` is used for nested allocations (if elements themselves contain
/// lists/arrays), see [recv_value].
#[async_recursion(?Send)]
async unsafe fn recv_elements<F>(
    stream: &TcpStream,
    elt_tag: Tag<'async_recursion>,
    length: usize,
    storage: *mut (),
    alloc: &(impl Fn(usize) -> F + 'async_recursion),
) -> Result<(), smoltcp::Error>
where
    F: Future<Output = *mut ()>,
{
    // List of simple types are special-cased in the protocol for performance.
    match elt_tag {
        Tag::Bool => {
            let dest = core::slice::from_raw_parts_mut(storage as *mut u8, length);
            proto_async::read_chunk(stream, dest).await?;
        }
        Tag::Int32 => {
            let ptr = storage as *mut u32;
            let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 4);
            proto_async::read_chunk(stream, dest).await?;
            let _ = dest;
            let dest = core::slice::from_raw_parts_mut(ptr, length);
            NativeEndian::from_slice_u32(dest);
        }
        Tag::Int64 | Tag::Float64 => {
            let ptr = storage as *mut u64;
            let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 8);
            proto_async::read_chunk(stream, dest).await?;
            let _ = dest;
            let dest = core::slice::from_raw_parts_mut(ptr, length);
            NativeEndian::from_slice_u64(dest);
        }
        _ => {
            let mut data = storage;
            for _ in 0..length {
                recv_value(stream, elt_tag, &mut data, alloc).await?
            }
        }
    }
    Ok(())
}

/// Reads (deserializes) a value of type `tag` from `stream`, writing the results to
/// the kernel-side buffer `data` (the passed pointer to which is incremented to point
/// past the just-received data). For nested allocations (lists/arrays), `alloc` is
/// invoked any number of times with the size of the required allocation as a parameter
/// (which is assumed to be correctly aligned for all payload types).
#[async_recursion(?Send)]
async unsafe fn recv_value<F>(
    stream: &TcpStream,
    tag: Tag<'async_recursion>,
    data: &mut *mut (),
    alloc: &(impl Fn(usize) -> F + 'async_recursion),
) -> Result<(), smoltcp::Error>
where
    F: Future<Output = *mut ()>,
{
    macro_rules! consume_value {
        ($ty:ty, | $ptr:ident | $map:expr) => {{
            let $ptr = align_ptr_mut::<$ty>(*data);
            *data = $ptr.offset(1) as *mut ();
            $map
        }};
    }

    match tag {
        Tag::None => Ok(()),
        Tag::Bool => consume_value!(i8, |ptr| {
            *ptr = proto_async::read_i8(stream).await?;
            Ok(())
        }),
        Tag::Int32 => consume_value!(i32, |ptr| {
            *ptr = proto_async::read_i32(stream).await?;
            Ok(())
        }),
        Tag::Int64 | Tag::Float64 => consume_value!(i64, |ptr| {
            *ptr = proto_async::read_i64(stream).await?;
            Ok(())
        }),
        Tag::String | Tag::Bytes | Tag::ByteArray => {
            consume_value!(CMutSlice<u8>, |ptr| {
                let length = proto_async::read_i32(stream).await? as usize;
                *ptr = CMutSlice::new(alloc(length).await as *mut u8, length);
                proto_async::read_chunk(stream, (*ptr).as_mut()).await?;
                Ok(())
            })
        }
        Tag::Tuple(it, arity) => {
            let alignment = tag.alignment();
            *data = round_up_mut(*data, alignment);
            let mut it = it.clone();
            for _ in 0..arity {
                let tag = it.next().expect("truncated tag");
                recv_value(stream, tag, data, alloc).await?
            }
            // Take into account any tail padding (if element(s) with largest alignment
            // are not at the end).
            *data = round_up_mut(*data, alignment);
            Ok(())
        }
        Tag::List(it) => {
            #[repr(C)]
            struct List {
                elements: *mut (),
                length: usize,
            }
            consume_value!(*mut List, |ptr_to_list| {
                let tag = it.clone().next().expect("truncated tag");
                let length = proto_async::read_i32(stream).await? as usize;

                // To avoid multiple kernel CPU roundtrips, use a single allocation for
                // both the pointer/length List (slice) and the backing storage for the
                // elements. We can assume that alloc() is aligned suitably, so just
                // need to take into account any extra padding required.
                // (Note: At the time of writing, there will never actually be any types
                // with alignment larger than 8 bytes, so storage_offset == 0 always.)
                let list_size = 4 + 4;
                let storage_offset = round_up(list_size, tag.alignment());
                let storage_size = tag.size() * length;

                let allocation = alloc(storage_offset + storage_size).await as *mut u8;
                *ptr_to_list = allocation as *mut List;
                let storage = allocation.offset(storage_offset as isize) as *mut ();

                (**ptr_to_list).length = length;
                (**ptr_to_list).elements = storage;
                recv_elements(stream, tag, length, storage, alloc).await
            })
        }
        Tag::Array(it, num_dims) => {
            consume_value!(*mut (), |buffer| {
                // Deserialize length along each dimension and compute total number of
                // elements.
                let mut total_len: usize = 1;
                for _ in 0..num_dims {
                    let len = proto_async::read_i32(stream).await? as usize;
                    total_len *= len;
                    consume_value!(usize, |ptr| *ptr = len)
                }

                // Allocate backing storage for elements; deserialize them.
                let elt_tag = it.clone().next().expect("truncated tag");
                *buffer = alloc(elt_tag.size() * total_len).await;
                recv_elements(stream, elt_tag, total_len, *buffer, alloc).await
            })
        }
        Tag::Range(it) => {
            *data = round_up_mut(*data, tag.alignment());
            let tag = it.clone().next().expect("truncated tag");
            recv_value(stream, tag, data, alloc).await?;
            recv_value(stream, tag, data, alloc).await?;
            recv_value(stream, tag, data, alloc).await?;
            Ok(())
        }
        Tag::Keyword(_) => unreachable!(),
        Tag::Object => unreachable!(),
    }
}

pub async fn recv_return<F>(
    stream: &TcpStream,
    tag_bytes: &[u8],
    data: *mut (),
    alloc: &impl Fn(usize) -> F,
) -> Result<(), smoltcp::Error>
where
    F: Future<Output = *mut ()>,
{
    let mut it = TagIterator::new(tag_bytes);
    trace!("recv ...->{}", it);

    let tag = it.next().expect("truncated tag");
    let mut data = data;
    unsafe { recv_value(stream, tag, &mut data, alloc).await? };

    Ok(())
}
