use alloc::{collections::BTreeMap, rc::Rc};
use core::{cell::RefCell, fmt};

use futures::{pin_mut, select_biased, FutureExt};
use libasync::{block_async, nb, smoltcp::TcpStream, task};
use libboard_artiq::drtio_routing;
use libboard_zynq::{smoltcp, time::Milliseconds, timer::GlobalTimer};
use libcortex_a9::mutex::Mutex;
use log::{debug, info, warn};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};
use void::Void;

use crate::proto_async::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NetworkError(smoltcp::Error),
    UnexpectedPattern,
    UnrecognizedPacket,
}

pub type Result<T> = core::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::NetworkError(error) => write!(f, "network error: {}", error),
            &Error::UnexpectedPattern => write!(f, "unexpected pattern"),
            &Error::UnrecognizedPacket => write!(f, "unrecognized packet"),
        }
    }
}

impl From<smoltcp::Error> for Error {
    fn from(error: smoltcp::Error) -> Self {
        Error::NetworkError(error)
    }
}

#[derive(Debug, FromPrimitive, ToPrimitive)]
enum HostMessage {
    MonitorProbe = 0,
    MonitorInjection = 3,
    Inject = 1,
    GetInjectionStatus = 2,
}

#[derive(Debug, FromPrimitive, ToPrimitive)]
enum DeviceMessage {
    MonitorStatus = 0,
    InjectionStatus = 1,
}

#[cfg(has_drtio)]
mod remote_moninj {
    use libboard_artiq::drtioaux_async;
    use log::error;

    use super::*;
    use crate::rtio_mgt::{drtio, drtio::Error as DrtioError};

    pub async fn read_probe(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &drtio_routing::RoutingTable,
        timer: GlobalTimer,
        linkno: u8,
        destination: u8,
        channel: i32,
        probe: i8,
    ) -> i64 {
        let reply = drtio::aux_transact(
            aux_mutex,
            linkno,
            routing_table,
            &drtioaux_async::Packet::MonitorRequest {
                destination: destination,
                channel: channel as _,
                probe: probe as _,
            },
            timer,
        )
        .await;
        match reply {
            Ok(drtioaux_async::Packet::MonitorReply { value }) => return value as i64,
            Ok(packet) => error!("received unexpected aux packet: {:?}", packet),
            Err(DrtioError::LinkDown) => {
                warn!("link is down");
            }
            Err(e) => error!("aux packet error ({})", e),
        }
        0
    }

    pub async fn inject(
        aux_mutex: &Rc<Mutex<bool>>,
        _routing_table: &drtio_routing::RoutingTable,
        _timer: GlobalTimer,
        linkno: u8,
        destination: u8,
        channel: i32,
        overrd: i8,
        value: i8,
    ) {
        let _lock = aux_mutex.async_lock().await;
        drtioaux_async::send(
            linkno,
            &drtioaux_async::Packet::InjectionRequest {
                destination: destination,
                channel: channel as _,
                overrd: overrd as _,
                value: value as _,
            },
        )
        .await
        .unwrap();
    }

    pub async fn read_injection_status(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &drtio_routing::RoutingTable,
        timer: GlobalTimer,
        linkno: u8,
        destination: u8,
        channel: i32,
        overrd: i8,
    ) -> i8 {
        let reply = drtio::aux_transact(
            aux_mutex,
            linkno,
            routing_table,
            &drtioaux_async::Packet::InjectionStatusRequest {
                destination: destination,
                channel: channel as _,
                overrd: overrd as _,
            },
            timer,
        )
        .await;
        match reply {
            Ok(drtioaux_async::Packet::InjectionStatusReply { value }) => return value as i8,
            Ok(packet) => error!("received unexpected aux packet: {:?}", packet),
            Err(DrtioError::LinkDown) => {
                warn!("link is down");
            }
            Err(e) => error!("aux packet error ({})", e),
        }
        0
    }
}

mod local_moninj {
    use libboard_artiq::pl::csr;

    pub fn read_probe(channel: i32, probe: i8) -> i64 {
        unsafe {
            csr::rtio_moninj::mon_chan_sel_write(channel as _);
            csr::rtio_moninj::mon_probe_sel_write(probe as _);
            csr::rtio_moninj::mon_value_update_write(1);
            csr::rtio_moninj::mon_value_read() as i64
        }
    }

    pub fn inject(channel: i32, overrd: i8, value: i8) {
        unsafe {
            csr::rtio_moninj::inj_chan_sel_write(channel as _);
            csr::rtio_moninj::inj_override_sel_write(overrd as _);
            csr::rtio_moninj::inj_value_write(value as _);
        }
    }

    pub fn read_injection_status(channel: i32, overrd: i8) -> i8 {
        unsafe {
            csr::rtio_moninj::inj_chan_sel_write(channel as _);
            csr::rtio_moninj::inj_override_sel_write(overrd as _);
            csr::rtio_moninj::inj_value_read() as i8
        }
    }
}

#[cfg(has_drtio)]
macro_rules! dispatch {
    ($timer:ident, $aux_mutex:ident, $routing_table:ident, $channel:expr, $func:ident $(, $param:expr)*) => {{
        let destination = ($channel >> 16) as u8;
        let channel = $channel;
        let hop = $routing_table.0[destination as usize][0];
        if hop == 0 {
            local_moninj::$func(channel.into(), $($param, )*)
        } else {
            let linkno = hop - 1 as u8;
            remote_moninj::$func($aux_mutex, $routing_table, $timer, linkno, destination, channel, $($param, )*).await
        }
    }}
}

#[cfg(not(has_drtio))]
macro_rules! dispatch {
    ($timer:ident, $aux_mutex:ident, $routing_table:ident, $channel:expr, $func:ident $(, $param:expr)*) => {{
        let channel = $channel as u16;
        local_moninj::$func(channel.into(), $($param, )*)
    }}
}

async fn handle_connection(
    stream: &TcpStream,
    timer: GlobalTimer,
    _aux_mutex: &Rc<Mutex<bool>>,
    _routing_table: &drtio_routing::RoutingTable,
) -> Result<()> {
    if !expect(&stream, b"ARTIQ moninj\n").await? {
        return Err(Error::UnexpectedPattern);
    }

    let mut probe_watch_list: BTreeMap<(i32, i8), Option<i64>> = BTreeMap::new();
    let mut inject_watch_list: BTreeMap<(i32, i8), Option<i8>> = BTreeMap::new();
    let mut next_check = timer.get_time();
    let timeout = |next_check: Milliseconds| -> nb::Result<(), Void> {
        if timer.get_time() < next_check {
            Err(nb::Error::WouldBlock)
        } else {
            Ok(())
        }
    };
    loop {
        // TODO: we don't need fuse() here.
        // remove after https://github.com/rust-lang/futures-rs/issues/1989 lands
        let read_message_f = read_i8(&stream).fuse();
        let next_check_c = next_check.clone();

        let timeout_f = block_async!(timeout(next_check_c)).fuse();
        pin_mut!(read_message_f, timeout_f);
        select_biased! {
            message = read_message_f => {
                let message: HostMessage = FromPrimitive::from_i8(message?)
                    .ok_or(Error::UnrecognizedPacket)?;
                match message {
                    HostMessage::MonitorProbe => {
                        let enable = read_bool(&stream).await?;
                        let channel = read_i32(&stream).await?;
                        let probe = read_i8(&stream).await?;
                        if enable {
                            let _ = probe_watch_list.entry((channel, probe)).or_insert(None);
                            debug!("START monitoring channel {}, probe {}", channel, probe);
                        } else {
                            let _ = probe_watch_list.remove(&(channel, probe));
                            debug!("END monitoring channel {}, probe {}", channel, probe);
                        }
                    },
                    HostMessage::MonitorInjection => {
                        let enable = read_bool(&stream).await?;
                        let channel = read_i32(&stream).await?;
                        let overrd = read_i8(&stream).await?;
                        if enable {
                            let _ = inject_watch_list.entry((channel, overrd)).or_insert(None);
                            debug!("START monitoring channel {}, overrd {}", channel, overrd);
                        } else {
                            let _ = inject_watch_list.remove(&(channel, overrd));
                            debug!("END monitoring channel {}, overrd {}", channel, overrd);
                        }
                    },
                    HostMessage::Inject => {
                        let channel = read_i32(&stream).await?;
                        let overrd = read_i8(&stream).await?;
                        let value = read_i8(&stream).await?;
                        dispatch!(timer, _aux_mutex, _routing_table, channel, inject, overrd, value);
                        debug!("INJECT channel {}, overrd {}, value {}", channel, overrd, value);
                    },
                    HostMessage::GetInjectionStatus => {
                        let channel = read_i32(&stream).await?;
                        let overrd = read_i8(&stream).await?;
                        let value = dispatch!(timer, _aux_mutex, _routing_table, channel, read_injection_status, overrd);
                        write_i8(&stream, DeviceMessage::InjectionStatus.to_i8().unwrap()).await?;
                        write_i32(&stream, channel).await?;
                        write_i8(&stream, overrd).await?;
                        write_i8(&stream, value).await?;
                    },
                }
            },
            _ = timeout_f => {
                for (&(channel, probe), previous) in probe_watch_list.iter_mut() {
                    let current = dispatch!(timer, _aux_mutex, _routing_table, channel, read_probe, probe);
                    if previous.is_none() || previous.unwrap() != current {
                        write_i8(&stream, DeviceMessage::MonitorStatus.to_i8().unwrap()).await?;
                        write_i32(&stream, channel).await?;
                        write_i8(&stream, probe).await?;
                        write_i64(&stream, current).await?;
                        *previous = Some(current);
                    }
                }
                for (&(channel, overrd), previous) in inject_watch_list.iter_mut() {
                    let current = dispatch!(timer, _aux_mutex, _routing_table, channel, read_injection_status, overrd);
                    if previous.is_none() || previous.unwrap() != current {
                        write_i8(&stream, DeviceMessage::InjectionStatus.to_i8().unwrap()).await?;
                        write_i32(&stream, channel).await?;
                        write_i8(&stream, overrd).await?;
                        write_i8(&stream, current).await?;
                        *previous = Some(current);
                    }
                }
                next_check = timer.get_time() + Milliseconds(200);
            }
        }
    }
}

pub fn start(
    timer: GlobalTimer,
    aux_mutex: &Rc<Mutex<bool>>,
    routing_table: &Rc<RefCell<drtio_routing::RoutingTable>>,
) {
    let aux_mutex = aux_mutex.clone();
    let routing_table = routing_table.clone();
    task::spawn(async move {
        loop {
            let aux_mutex = aux_mutex.clone();
            let routing_table = routing_table.clone();
            let stream = TcpStream::accept(1383, 2048, 2048).await.unwrap();
            task::spawn(async move {
                info!("received connection");
                let routing_table = routing_table.borrow();
                let result = handle_connection(&stream, timer, &aux_mutex, &routing_table).await;
                match result {
                    Err(Error::NetworkError(smoltcp::Error::Finished)) => info!("peer closed connection"),
                    Err(error) => warn!("connection terminated: {}", error),
                    _ => (),
                }
                let _ = stream.flush().await;
                let _ = stream.abort().await;
            });
        }
    });
}
