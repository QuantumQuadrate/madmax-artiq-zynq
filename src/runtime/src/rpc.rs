use core::str;
use core::future::Future;
use cslice::{CSlice, CMutSlice};
use log::trace;
use byteorder::{NativeEndian, ByteOrder};

use core_io::{Write, Error};
use libboard_zynq::smoltcp;
use libasync::smoltcp::TcpStream;
use alloc::boxed::Box;
use async_recursion::async_recursion;

use crate::proto_core_io::ProtoWrite;
use crate::proto_async;
use self::tag::{Tag, TagIterator, split_tag};

#[inline]
fn alignment_offset(alignment: isize, ptr: isize) -> isize {
    (alignment - ptr % alignment) % alignment
}

unsafe fn align_ptr<T>(ptr: *const ()) -> *const T {
    let alignment = core::mem::align_of::<T>() as isize;
    let fix = alignment_offset(alignment, ptr as isize);
    ((ptr as isize) + fix) as *const T
}

unsafe fn align_ptr_mut<T>(ptr: *mut ()) -> *mut T {
    let alignment = core::mem::align_of::<T>() as isize;
    let fix = alignment_offset(alignment, ptr as isize);
    ((ptr as isize) + fix) as *mut T
}

#[async_recursion(?Send)]
async unsafe fn recv_value<F>(stream: &TcpStream, tag: Tag<'async_recursion>, data: &mut *mut (),
                              alloc: &(impl Fn(usize) -> F + 'async_recursion))
                             -> Result<(), smoltcp::Error>
                          where F: Future<Output=*mut ()>
{
    macro_rules! consume_value {
        ($ty:ty, |$ptr:ident| $map:expr) => ({
            let $ptr = align_ptr_mut::<$ty>(*data);
            *data = $ptr.offset(1) as *mut ();
            $map
        })
    }

    match tag {
        Tag::None => Ok(()),
        Tag::Bool =>
            consume_value!(i8, |ptr| {
                *ptr = proto_async::read_i8(stream).await?;
                Ok(())
            }),
        Tag::Int32 =>
            consume_value!(i32, |ptr| {
                *ptr = proto_async::read_i32(stream).await?;
                Ok(())
            }),
        Tag::Int64 | Tag::Float64 =>
            consume_value!(i64, |ptr| {
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
            *data = (*data).offset(alignment_offset(tag.alignment() as isize, *data as isize));
            let mut it = it.clone();
            for _ in 0..arity {
                let tag = it.next().expect("truncated tag");
                recv_value(stream, tag, data, alloc).await?;
            }
            Ok(())
        }
        Tag::List(it) => {
            #[repr(C)]
            struct List { elements: *mut (), length: u32 }
            consume_value!(List, |ptr| {
                let length = proto_async::read_i32(stream).await? as usize;
                (*ptr).length  = length as u32;
                let tag = it.clone().next().expect("truncated tag");
                let data_size = tag.size() * length as usize +
                    match tag {
                        Tag::Int64 | Tag::Float64 => 4,
                        _ => 0
                    };
                let mut data = alloc(data_size).await;

                let alignment = tag.alignment();
                data = data.offset(alignment_offset(alignment as isize, data as isize));
                (*ptr).elements = data;
                match tag {
                    Tag::Bool => {
                        let ptr = data as *mut u8;
                        let dest = core::slice::from_raw_parts_mut(ptr, length);
                        proto_async::read_chunk(stream, dest).await?;
                    },
                    Tag::Int32 => {
                        let ptr = data as *mut u32;
                        // reading as raw bytes and do endianness conversion later
                        let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 4);
                        proto_async::read_chunk(stream, dest).await?;
                        drop(dest);
                        let dest = core::slice::from_raw_parts_mut(ptr, length);
                        NativeEndian::from_slice_u32(dest);
                    },
                    Tag::Int64 | Tag::Float64 => {
                        let ptr = data as *mut u64;
                        let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 8);
                        proto_async::read_chunk(stream, dest).await?;
                        drop(dest);
                        let dest = core::slice::from_raw_parts_mut(ptr, length);
                        NativeEndian::from_slice_u64(dest);
                    },
                    _ => {
                        for _ in 0..(*ptr).length as usize {
                            recv_value(stream, tag, &mut data, alloc).await?
                        }
                    }
                }
                Ok(())
            })
        }
        Tag::Array(it, num_dims) => {
            consume_value!(*mut (), |buffer| {
                let mut total_len: u32 = 1;
                for _ in 0..num_dims {
                    let len = proto_async::read_i32(stream).await? as u32;
                    total_len *= len;
                    consume_value!(u32, |ptr| *ptr = len )
                }

                let elt_tag = it.clone().next().expect("truncated tag");
                let data_size = elt_tag.size() * total_len as usize +
                    match elt_tag {
                        Tag::Int64 | Tag::Float64 => 4,
                        _ => 0
                    };
                let mut data = alloc(data_size).await;

                let alignment = tag.alignment();
                data = data.offset(alignment_offset(alignment as isize, data as isize));
                *buffer = data;
                let length = total_len as usize;
                match elt_tag {
                    Tag::Bool => {
                        let ptr = data as *mut u8;
                        let dest = core::slice::from_raw_parts_mut(ptr, length);
                        proto_async::read_chunk(stream, dest).await?;
                    },
                    Tag::Int32 => {
                        let ptr = data as *mut u32;
                        let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 4);
                        proto_async::read_chunk(stream, dest).await?;
                        drop(dest);
                        let dest = core::slice::from_raw_parts_mut(ptr, length);
                        NativeEndian::from_slice_u32(dest);
                    },
                    Tag::Int64 | Tag::Float64 => {
                        let ptr = data as *mut u64;
                        let dest = core::slice::from_raw_parts_mut(ptr as *mut u8, length * 8);
                        proto_async::read_chunk(stream, dest).await?;
                        drop(dest);
                        let dest = core::slice::from_raw_parts_mut(ptr, length);
                        NativeEndian::from_slice_u64(dest);
                    },
                    _ => {
                        for _ in 0..length {
                            recv_value(stream, elt_tag, &mut data, alloc).await?
                        }
                    }
                }
                Ok(())
            })
        }
        Tag::Range(it) => {
            *data = (*data).offset(alignment_offset(tag.alignment() as isize, *data as isize));
            let tag = it.clone().next().expect("truncated tag");
            recv_value(stream, tag, data, alloc).await?;
            recv_value(stream, tag, data, alloc).await?;
            recv_value(stream, tag, data, alloc).await?;
            Ok(())
        }
        Tag::Keyword(_) => unreachable!(),
        Tag::Object => unreachable!()
    }
}

pub async fn recv_return<F>(stream: &TcpStream, tag_bytes: &[u8], data: *mut (),
                            alloc: &impl Fn(usize) -> F)
                           -> Result<(), smoltcp::Error>
                        where F: Future<Output=*mut ()>
{
    let mut it = TagIterator::new(tag_bytes);
    trace!("recv ...->{}", it);

    let tag = it.next().expect("truncated tag");
    let mut data = data;
    unsafe { recv_value(stream, tag, &mut data, alloc).await? };

    Ok(())
}

unsafe fn send_value<W>(writer: &mut W, tag: Tag, data: &mut *const ())
                       -> Result<(), Error>
    where W: Write + ?Sized
{
    macro_rules! consume_value {
        ($ty:ty, |$ptr:ident| $map:expr) => ({
            let $ptr = align_ptr::<$ty>(*data);
            *data = $ptr.offset(1) as *const ();
            $map
        })
    }

    writer.write_u8(tag.as_u8())?;
    match tag {
        Tag::None => Ok(()),
        Tag::Bool =>
            consume_value!(u8, |ptr|
                writer.write_u8(*ptr)),
        Tag::Int32 =>
            consume_value!(u32, |ptr|
                writer.write_u32(*ptr)),
        Tag::Int64 | Tag::Float64 =>
            consume_value!(u64, |ptr|
                writer.write_u64(*ptr)),
        Tag::String =>
            consume_value!(CSlice<u8>, |ptr|
                writer.write_string(str::from_utf8((*ptr).as_ref()).unwrap())),
        Tag::Bytes | Tag::ByteArray =>
            consume_value!(CSlice<u8>, |ptr|
                writer.write_bytes((*ptr).as_ref())),
        Tag::Tuple(it, arity) => {
            let mut it = it.clone();
            writer.write_u8(arity)?;
            for _ in 0..arity {
                let tag = it.next().expect("truncated tag");
                send_value(writer, tag, data)?
            }
            Ok(())
        }
        Tag::List(it) => {
            #[repr(C)]
            struct List { elements: *const (), length: u32 }
            consume_value!(List, |ptr| {
                let length = (*ptr).length as isize;
                writer.write_u32((*ptr).length)?;
                let tag = it.clone().next().expect("truncated tag");
                let mut data = (*ptr).elements;
                writer.write_u8(tag.as_u8())?;
                match tag {
                    Tag::Bool => {
                        // we can pretend this is u8...
                        let ptr1 = align_ptr::<u8>(data);
                        let slice = core::slice::from_raw_parts(ptr1, length as usize);
                        writer.write_all(slice)?;
                    },
                    Tag::Int32 => {
                        let ptr1 = align_ptr::<i32>(data);
                        let slice = core::slice::from_raw_parts(ptr1 as *const u8, length as usize * 4);
                        writer.write_all(slice)?;
                    },
                    Tag::Int64 | Tag::Float64 => {
                        let ptr1 = align_ptr::<i64>(data);
                        let slice = core::slice::from_raw_parts(ptr1 as *const u8, length as usize * 8);
                        writer.write_all(slice)?;
                    },
                    // non-primitive types, not sure if this would happen but we can handle it...
                    _ => {
                        for _ in 0..length {
                            send_value(writer, tag, &mut data)?;
                        }
                    }
                };
                Ok(())
            })
        }
        Tag::Array(it, num_dims) => {
            writer.write_u8(num_dims)?;
            consume_value!(*const(), |buffer| {
                let elt_tag = it.clone().next().expect("truncated tag");

                let mut total_len = 1;
                for _ in 0..num_dims {
                    consume_value!(u32, |len| {
                        writer.write_u32(*len)?;
                        total_len *= *len;
                    })
                }
                let mut data = *buffer;
                let length = total_len as isize;
                writer.write_u8(elt_tag.as_u8())?;
                match elt_tag {
                    Tag::Bool => {
                        let ptr1 = align_ptr::<u8>(data);
                        let slice = core::slice::from_raw_parts(ptr1, length as usize);
                        writer.write_all(slice)?;
                    },
                    Tag::Int32 => {
                        let ptr1 = align_ptr::<i32>(data);
                        let slice = core::slice::from_raw_parts(ptr1 as *const u8, length as usize * 4);
                        writer.write_all(slice)?;
                    },
                    Tag::Int64 | Tag::Float64 => {
                        let ptr1 = align_ptr::<i64>(data);
                        let slice = core::slice::from_raw_parts(ptr1 as *const u8, length as usize * 8);
                        writer.write_all(slice)?;
                    },
                    // non-primitive types, not sure if this would happen but we can handle it...
                    _ => {
                        for _ in 0..length {
                            send_value(writer, elt_tag, &mut data)?;
                        }
                    }
                };
                Ok(())
            })
        }
        Tag::Range(it) => {
            let tag = it.clone().next().expect("truncated tag");
            send_value(writer, tag, data)?;
            send_value(writer, tag, data)?;
            send_value(writer, tag, data)?;
            Ok(())
        }
        Tag::Keyword(it) => {
            #[repr(C)]
            struct Keyword<'a> { name: CSlice<'a, u8> }
            consume_value!(Keyword, |ptr| {
                writer.write_string(str::from_utf8((*ptr).name.as_ref()).unwrap())?;
                let tag = it.clone().next().expect("truncated tag");
                let mut data = ptr.offset(1) as *const ();
                send_value(writer, tag, &mut data)
            })
            // Tag::Keyword never appears in composite types, so we don't have
            // to accurately advance data.
        }
        Tag::Object => {
            #[repr(C)]
            struct Object { id: u32 }
            consume_value!(*const Object, |ptr|
                writer.write_u32((**ptr).id))
        }
    }
}

pub fn send_args<W>(writer: &mut W, service: u32, tag_bytes: &[u8], data: *const *const ())
                   -> Result<(), Error>
    where W: Write + ?Sized
{
    let (arg_tags_bytes, return_tag_bytes) = split_tag(tag_bytes);

    let mut args_it = TagIterator::new(arg_tags_bytes);
    let return_it = TagIterator::new(return_tag_bytes);
    trace!("send<{}>({})->{}", service, args_it, return_it);

    writer.write_u32(service)?;
    for index in 0.. {
        if let Some(arg_tag) = args_it.next() {
            let mut data = unsafe { *data.offset(index) };
            unsafe { send_value(writer, arg_tag, &mut data)? };
        } else {
            break
        }
    }
    writer.write_u8(0)?;
    writer.write_bytes(return_tag_bytes)?;

    Ok(())
}

mod tag {
    use core::fmt;

    pub fn split_tag(tag_bytes: &[u8]) -> (&[u8], &[u8]) {
        let tag_separator =
            tag_bytes.iter()
                     .position(|&b| b == b':')
                     .expect("tag without a return separator");
        let (arg_tags_bytes, rest) = tag_bytes.split_at(tag_separator);
        let return_tag_bytes = &rest[1..];
        (arg_tags_bytes, return_tag_bytes)
    }

    #[derive(Debug, Clone, Copy)]
    pub enum Tag<'a> {
        None,
        Bool,
        Int32,
        Int64,
        Float64,
        String,
        Bytes,
        ByteArray,
        Tuple(TagIterator<'a>, u8),
        List(TagIterator<'a>),
        Array(TagIterator<'a>, u8),
        Range(TagIterator<'a>),
        Keyword(TagIterator<'a>),
        Object
    }

    impl<'a> Tag<'a> {
        pub fn as_u8(self) -> u8 {
            match self {
                Tag::None => b'n',
                Tag::Bool => b'b',
                Tag::Int32 => b'i',
                Tag::Int64 => b'I',
                Tag::Float64 => b'f',
                Tag::String => b's',
                Tag::Bytes => b'B',
                Tag::ByteArray => b'A',
                Tag::Tuple(_, _) => b't',
                Tag::List(_) => b'l',
                Tag::Array(_, _) => b'a',
                Tag::Range(_) => b'r',
                Tag::Keyword(_) => b'k',
                Tag::Object => b'O',
            }
        }

        pub fn alignment(self) -> usize {
            use cslice::CSlice;
            match self {
                Tag::None => 1,
                Tag::Bool => core::mem::align_of::<u8>(),
                Tag::Int32 => core::mem::align_of::<i32>(),
                Tag::Int64 => core::mem::align_of::<i64>(),
                Tag::Float64 => core::mem::align_of::<f64>(),
                // struct type: align to largest element
                Tag::Tuple(it, arity) => {
                    let it = it.clone();
                    it.take(arity.into()).map(|t| t.alignment()).max().unwrap()
                },
                Tag::Range(it) => {
                    let it = it.clone();
                    it.take(3).map(|t| t.alignment()).max().unwrap()
                }
                // CSlice basically
                Tag::Bytes | Tag::String | Tag::ByteArray | Tag::List(_) =>
                    core::mem::align_of::<CSlice<()>>(),
                // array buffer is allocated, so no need for alignment first
                Tag::Array(_, _) => 1,
                // will not be sent from the host
                _ => unreachable!("unexpected tag from host")
            }
        }

        pub fn size(self) -> usize {
            use super::alignment_offset;
            match self {
                Tag::None => 0,
                Tag::Bool => 1,
                Tag::Int32 => 4,
                Tag::Int64 => 8,
                Tag::Float64 => 8,
                Tag::String => 8,
                Tag::Bytes => 8,
                Tag::ByteArray => 8,
                Tag::Tuple(it, arity) => {
                    let mut size = 0;
                    let mut it = it.clone();
                    for _ in 0..arity {
                        let tag = it.next().expect("truncated tag");
                        size += tag.size();
                        // includes padding
                        size += alignment_offset(tag.alignment() as isize, size as isize) as usize;
                    }
                    size
                }
                Tag::List(_) => 8,
                Tag::Array(_, num_dims) => 4 * (1 + num_dims as usize),
                Tag::Range(it) => {
                    let tag = it.clone().next().expect("truncated tag");
                    tag.size() * 3
                }
                Tag::Keyword(_) => unreachable!(),
                Tag::Object => unreachable!(),
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct TagIterator<'a> {
        data: &'a [u8]
    }

    impl<'a> TagIterator<'a> {
        pub fn new(data: &'a [u8]) -> TagIterator<'a> {
            TagIterator { data }
        }


        fn sub(&mut self, count: u8) -> TagIterator<'a> {
            let data = self.data;
            for _ in 0..count {
                self.next().expect("truncated tag");
            }
            TagIterator { data: &data[..(data.len() - self.data.len())] }
        }
    }

    impl<'a> core::iter::Iterator for TagIterator<'a> {
        type Item = Tag<'a>;

        fn next(&mut self) -> Option<Tag<'a>> {
            if self.data.len() == 0 {
                return None
            }

            let tag_byte = self.data[0];
            self.data = &self.data[1..];
            Some(match tag_byte {
                b'n' => Tag::None,
                b'b' => Tag::Bool,
                b'i' => Tag::Int32,
                b'I' => Tag::Int64,
                b'f' => Tag::Float64,
                b's' => Tag::String,
                b'B' => Tag::Bytes,
                b'A' => Tag::ByteArray,
                b't' => {
                    let count = self.data[0];
                    self.data = &self.data[1..];
                    Tag::Tuple(self.sub(count), count)
                }
                b'l' => Tag::List(self.sub(1)),
                b'a' => {
                    let count = self.data[0];
                    self.data = &self.data[1..];
                    Tag::Array(self.sub(1), count)
                }
                b'r' => Tag::Range(self.sub(1)),
                b'k' => Tag::Keyword(self.sub(1)),
                b'O' => Tag::Object,
                _    => unreachable!()
            })
        }
    }

    impl<'a> fmt::Display for TagIterator<'a> {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let mut it = self.clone();
            let mut first = true;
            while let Some(tag) = it.next() {
                if first {
                    first = false
                } else {
                    write!(f, ", ")?
                }

                match tag {
                    Tag::None =>
                        write!(f, "None")?,
                    Tag::Bool =>
                        write!(f, "Bool")?,
                    Tag::Int32 =>
                        write!(f, "Int32")?,
                    Tag::Int64 =>
                        write!(f, "Int64")?,
                    Tag::Float64 =>
                        write!(f, "Float64")?,
                    Tag::String =>
                        write!(f, "String")?,
                    Tag::Bytes =>
                        write!(f, "Bytes")?,
                    Tag::ByteArray =>
                        write!(f, "ByteArray")?,
                    Tag::Tuple(it, _) => {
                        write!(f, "Tuple(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::List(it) => {
                        write!(f, "List(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::Array(it, num_dims) => {
                        write!(f, "Array(")?;
                        it.fmt(f)?;
                        write!(f, ", {})", num_dims)?;
                    }
                    Tag::Range(it) => {
                        write!(f, "Range(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::Keyword(it) => {
                        write!(f, "Keyword(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::Object =>
                        write!(f, "Object")?,
                }
            }

            Ok(())
        }
    }
}
