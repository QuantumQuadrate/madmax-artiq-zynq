use alloc::boxed::Box; // for async_recursion

use async_recursion::async_recursion;
use byteorder::{ByteOrder, NativeEndian};
use core_io::Error;
use cslice::CMutSlice;
use io::ProtoRead;
use ksupport::rpc::{tag::{Tag, TagIterator},
                    *};
use log::trace;

#[async_recursion(?Send)]
async unsafe fn recv_elements<R: ProtoRead>(
    reader: &mut R,
    elt_tag: Tag<'async_recursion>,
    length: usize,
    storage: *mut (),
    alloc: &mut (impl AsyncFnMut(usize) -> *mut () + 'async_recursion),
) -> Result<(), Error> {
    match elt_tag {
        Tag::Bool => {
            let dest = core::slice::from_raw_parts_mut(storage as *mut u8, length);
            reader.read_exact(dest)?;
        }
        Tag::Int32 => {
            let ptr = storage as *mut u32;
            let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 4);
            reader.read_exact(dest)?;
            let _ = dest;
            let dest = core::slice::from_raw_parts_mut(ptr, length);
            NativeEndian::from_slice_u32(dest);
        }
        Tag::Int64 | Tag::Float64 => {
            let ptr = storage as *mut u64;
            let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 8);
            reader.read_exact(dest)?;
            let _ = dest;
            let dest = core::slice::from_raw_parts_mut(ptr, length);
            NativeEndian::from_slice_u64(dest);
        }
        _ => {
            let mut data = storage;
            for _ in 0..length {
                recv_value(reader, elt_tag, &mut data, alloc).await?
            }
        }
    }
    Ok(())
}

#[async_recursion(?Send)]
async unsafe fn recv_value<R: ProtoRead>(
    reader: &mut R,
    tag: Tag<'async_recursion>,
    data: &mut *mut (),
    alloc: &mut (impl AsyncFnMut(usize) -> *mut () + 'async_recursion),
) -> Result<(), Error> {
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
            *ptr = reader.read_u8()? as i8;
            Ok(())
        }),
        Tag::Int32 => consume_value!(i32, |ptr| {
            *ptr = reader.read_u32::<NativeEndian>()? as i32;
            Ok(())
        }),
        Tag::Int64 | Tag::Float64 => consume_value!(i64, |ptr| {
            *ptr = reader.read_u64::<NativeEndian>()? as i64;
            Ok(())
        }),
        Tag::String | Tag::Bytes | Tag::ByteArray => {
            consume_value!(CMutSlice<u8>, |ptr| {
                let length = reader.read_u32::<NativeEndian>()? as usize;
                *ptr = CMutSlice::new(alloc(length).await as *mut u8, length);
                reader.read_exact((*ptr).as_mut())?;
                Ok(())
            })
        }
        Tag::Tuple(it, arity) => {
            let alignment = tag.alignment();
            *data = round_up_mut(*data, alignment);
            let mut it = it.clone();
            for _ in 0..arity {
                let tag = it.next().expect("truncated tag");
                recv_value(reader, tag, data, alloc).await?
            }
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
                let length = reader.read_u32::<NativeEndian>()? as usize;

                let list_size = 4 + 4;
                let storage_offset = round_up(list_size, tag.alignment());
                let storage_size = tag.size() * length;

                let allocation = alloc(storage_offset + storage_size).await as *mut u8;
                *ptr_to_list = allocation as *mut List;
                let storage = allocation.offset(storage_offset as isize) as *mut ();

                (**ptr_to_list).length = length;
                (**ptr_to_list).elements = storage;
                recv_elements(reader, tag, length, storage, alloc).await
            })
        }
        Tag::Array(it, num_dims) => {
            consume_value!(*mut (), |buffer| {
                let mut total_len: usize = 1;
                for _ in 0..num_dims {
                    let len = reader.read_u32::<NativeEndian>()? as usize;
                    total_len *= len;
                    consume_value!(usize, |ptr| *ptr = len)
                }

                let elt_tag = it.clone().next().expect("truncated tag");
                *buffer = alloc(elt_tag.size() * total_len).await;
                recv_elements(reader, elt_tag, total_len, *buffer, alloc).await
            })
        }
        Tag::Range(it) => {
            *data = round_up_mut(*data, tag.alignment());
            let tag = it.clone().next().expect("truncated tag");
            recv_value(reader, tag, data, alloc).await?;
            recv_value(reader, tag, data, alloc).await?;
            recv_value(reader, tag, data, alloc).await?;
            Ok(())
        }
        Tag::Keyword(_) => unreachable!(),
        Tag::Object => unreachable!(),
    }
}

pub async fn recv_return<'a, 'b, R>(
    reader: &mut R,
    tag_bytes: &'a [u8],
    data: *mut (),
    alloc: &'b mut impl AsyncFnMut(usize) -> *mut (),
) -> Result<&'a [u8], Error>
where
    R: ProtoRead,
{
    let mut it = TagIterator::new(tag_bytes);
    trace!("recv ...->{}", it);

    let tag = it.next().expect("truncated tag");
    let mut data = data;
    unsafe { recv_value(reader, tag, &mut data, alloc).await? };

    Ok(it.data)
}
