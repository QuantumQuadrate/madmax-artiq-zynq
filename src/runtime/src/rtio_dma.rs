use alloc::{collections::BTreeMap, rc::Rc, string::String, vec::Vec};
#[cfg(has_drtio)]
use core::mem;

use ksupport::kernel::DmaRecorder;
#[cfg(has_drtio)]
use libasync::task;
use libboard_artiq::drtio_routing::RoutingTable;
use libboard_zynq::timer::GlobalTimer;
use libcortex_a9::{cache::dcci_slice, mutex::Mutex};

const ALIGNMENT: usize = 16 * 8;

static DMA_RECORD_STORE: Mutex<BTreeMap<String, (u32, Vec<u8>, i64)>> = Mutex::new(BTreeMap::new());

#[cfg(has_drtio)]
pub mod remote_dma {
    use libboard_zynq::time::Milliseconds;
    use log::error;

    use super::*;
    use crate::rtio_mgt::drtio;

    #[derive(Debug, PartialEq, Clone)]
    pub enum RemoteState {
        NotLoaded,
        Loaded,
        PlaybackEnded { error: u8, channel: u32, timestamp: u64 },
    }
    #[derive(Debug, Clone)]
    struct RemoteTrace {
        trace: Vec<u8>,
        pub state: RemoteState,
    }

    impl From<Vec<u8>> for RemoteTrace {
        fn from(trace: Vec<u8>) -> Self {
            RemoteTrace {
                trace: trace,
                state: RemoteState::NotLoaded,
            }
        }
    }

    impl RemoteTrace {
        pub fn get_trace(&self) -> &Vec<u8> {
            &self.trace
        }
    }

    // represents all traces for a given ID
    struct TraceSet {
        id: u32,
        done_count: Mutex<usize>,
        traces: Mutex<BTreeMap<u8, RemoteTrace>>,
    }

    impl TraceSet {
        pub fn new(id: u32, traces: BTreeMap<u8, Vec<u8>>) -> TraceSet {
            let mut trace_map: BTreeMap<u8, RemoteTrace> = BTreeMap::new();
            for (destination, trace) in traces {
                trace_map.insert(destination, trace.into());
            }
            TraceSet {
                id: id,
                done_count: Mutex::new(0),
                traces: Mutex::new(trace_map),
            }
        }

        pub async fn await_done(&self, timeout: Option<u64>, timer: GlobalTimer) -> Result<RemoteState, &'static str> {
            let timeout_ms = Milliseconds(timeout.unwrap_or(10_000));
            let limit = timer.get_time() + timeout_ms;
            while (timer.get_time() < limit)
                & (*(self.done_count.async_lock().await) < self.traces.async_lock().await.len())
            {
                task::r#yield().await;
            }
            if timer.get_time() >= limit {
                error!("Remote DMA await done timed out");
                return Err("Timed out waiting for results.");
            }
            let mut playback_state: RemoteState = RemoteState::PlaybackEnded {
                error: 0,
                channel: 0,
                timestamp: 0,
            };
            let mut lock = self.traces.async_lock().await;
            let trace_iter = lock.iter_mut();
            for (_dest, trace) in trace_iter {
                match trace.state {
                    RemoteState::PlaybackEnded {
                        error: e,
                        channel: _c,
                        timestamp: _ts,
                    } => {
                        if e != 0 {
                            playback_state = trace.state.clone();
                        }
                    }
                    _ => (),
                }
                trace.state = RemoteState::Loaded;
            }
            Ok(playback_state)
        }

        pub async fn upload_traces(
            &mut self,
            aux_mutex: &Rc<Mutex<bool>>,
            routing_table: &RoutingTable,
            timer: GlobalTimer,
        ) {
            let mut lock = self.traces.async_lock().await;
            let trace_iter = lock.iter_mut();
            for (destination, trace) in trace_iter {
                match drtio::ddma_upload_trace(
                    aux_mutex,
                    routing_table,
                    timer,
                    self.id,
                    *destination,
                    trace.get_trace(),
                )
                .await
                {
                    Ok(_) => trace.state = RemoteState::Loaded,
                    Err(e) => error!("Error adding DMA trace on destination {}: {}", destination, e),
                }
            }
            *(self.done_count.async_lock().await) = 0;
        }

        pub async fn erase(&mut self, aux_mutex: &Rc<Mutex<bool>>, routing_table: &RoutingTable, timer: GlobalTimer) {
            let lock = self.traces.async_lock().await;
            let trace_iter = lock.keys();
            for destination in trace_iter {
                match drtio::ddma_send_erase(aux_mutex, routing_table, timer, self.id, *destination).await {
                    Ok(_) => (),
                    Err(e) => error!("Error adding DMA trace on destination {}: {}", destination, e),
                }
            }
        }

        pub async fn playback_done(&mut self, source: u8, error: u8, channel: u32, timestamp: u64) {
            let mut traces_locked = self.traces.async_lock().await;
            let trace = traces_locked.get_mut(&source).unwrap();
            trace.state = RemoteState::PlaybackEnded {
                error: error,
                channel: channel,
                timestamp: timestamp,
            };
            *(self.done_count.async_lock().await) += 1;
        }

        pub async fn playback(
            &self,
            aux_mutex: &Rc<Mutex<bool>>,
            routing_table: &RoutingTable,
            timer: GlobalTimer,
            timestamp: u64,
        ) {
            let mut dest_list: Vec<u8> = Vec::new();
            {
                let lock = self.traces.async_lock().await;
                let trace_iter = lock.iter();
                for (dest, trace) in trace_iter {
                    if trace.state != RemoteState::Loaded {
                        error!("Destination {} not ready for DMA, state: {:?}", dest, trace.state);
                        continue;
                    }
                    dest_list.push(dest.clone());
                }
            }
            // mutex lock must be dropped before sending a playback request to avoid a deadlock,
            // if PlaybackStatus is sent from another satellite and the state must be updated.
            for destination in dest_list {
                match drtio::ddma_send_playback(aux_mutex, routing_table, timer, self.id, destination, timestamp).await
                {
                    Ok(_) => (),
                    Err(e) => error!("Error during remote DMA playback: {}", e),
                }
            }
        }

        pub async fn destination_changed(
            &mut self,
            aux_mutex: &Rc<Mutex<bool>>,
            routing_table: &RoutingTable,
            timer: GlobalTimer,
            destination: u8,
            up: bool,
        ) {
            // update state of the destination, resend traces if it's up
            if let Some(trace) = self.traces.async_lock().await.get_mut(&destination) {
                if up {
                    match drtio::ddma_upload_trace(
                        aux_mutex,
                        routing_table,
                        timer,
                        self.id,
                        destination,
                        trace.get_trace(),
                    )
                    .await
                    {
                        Ok(_) => trace.state = RemoteState::Loaded,
                        Err(e) => error!("Error adding DMA trace on destination {}: {}", destination, e),
                    }
                } else {
                    trace.state = RemoteState::NotLoaded;
                }
            }
        }

        pub async fn is_empty(&self) -> bool {
            self.traces.async_lock().await.is_empty()
        }
    }

    static mut TRACES: BTreeMap<u32, TraceSet> = BTreeMap::new();

    pub fn add_traces(id: u32, traces: BTreeMap<u8, Vec<u8>>) {
        unsafe { TRACES.insert(id, TraceSet::new(id, traces)) };
    }

    pub async fn await_done(id: u32, timeout: Option<u64>, timer: GlobalTimer) -> Result<RemoteState, &'static str> {
        let trace_set = unsafe { TRACES.get_mut(&id).unwrap() };
        trace_set.await_done(timeout, timer).await
    }

    pub async fn erase(aux_mutex: &Rc<Mutex<bool>>, routing_table: &RoutingTable, timer: GlobalTimer, id: u32) {
        let trace_set = unsafe { TRACES.get_mut(&id).unwrap() };
        trace_set.erase(aux_mutex, routing_table, timer).await;
        unsafe {
            TRACES.remove(&id);
        }
    }

    pub async fn upload_traces(aux_mutex: &Rc<Mutex<bool>>, routing_table: &RoutingTable, timer: GlobalTimer, id: u32) {
        let trace_set = unsafe { TRACES.get_mut(&id).unwrap() };
        trace_set.upload_traces(aux_mutex, routing_table, timer).await;
    }

    pub async fn playback(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        id: u32,
        timestamp: u64,
    ) {
        let trace_set = unsafe { TRACES.get_mut(&id).unwrap() };
        trace_set.playback(aux_mutex, routing_table, timer, timestamp).await;
    }

    pub async fn playback_done(id: u32, destination: u8, error: u8, channel: u32, timestamp: u64) {
        let trace_set = unsafe { TRACES.get_mut(&id).unwrap() };
        trace_set.playback_done(destination, error, channel, timestamp).await;
    }

    pub async fn destination_changed(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        destination: u8,
        up: bool,
    ) {
        let trace_iter = unsafe { TRACES.values_mut() };
        for trace_set in trace_iter {
            trace_set
                .destination_changed(aux_mutex, routing_table, timer, destination, up)
                .await;
        }
    }

    pub async fn has_remote_traces(id: u32) -> bool {
        let trace_set = unsafe { TRACES.get_mut(&id).unwrap() };
        !(trace_set.is_empty().await)
    }
}

pub async fn put_record(
    _aux_mutex: &Rc<Mutex<bool>>,
    _routing_table: &RoutingTable,
    _timer: GlobalTimer,
    mut recorder: DmaRecorder,
) -> u32 {
    #[cfg(has_drtio)]
    let mut remote_traces: BTreeMap<u8, Vec<u8>> = BTreeMap::new();

    #[cfg(has_drtio)]
    if recorder.enable_ddma {
        let mut local_trace: Vec<u8> = Vec::new();
        // analyze each entry and put in proper buckets, as the kernel core
        // sends whole chunks, to limit comms/kernel CPU communication,
        // and as only comms core has access to varios DMA buffers.
        let mut ptr = 0;
        recorder.buffer.push(0);
        while recorder.buffer[ptr] != 0 {
            // ptr + 3 = tgt >> 24 (destination)
            let len = recorder.buffer[ptr] as usize;
            let destination = recorder.buffer[ptr + 3];
            if destination == 0 {
                local_trace.extend(&recorder.buffer[ptr..ptr + len]);
            } else {
                if let Some(remote_trace) = remote_traces.get_mut(&destination) {
                    remote_trace.extend(&recorder.buffer[ptr..ptr + len]);
                } else {
                    remote_traces.insert(destination, recorder.buffer[ptr..ptr + len].to_vec());
                }
            }
            // and jump to the next event
            ptr += len;
        }
        mem::swap(&mut recorder.buffer, &mut local_trace);
    }
    // trailing zero to indicate end of buffer
    recorder.buffer.push(0);
    recorder.buffer.reserve(ALIGNMENT - 1);
    let original_length = recorder.buffer.len();
    let padding = ALIGNMENT - recorder.buffer.as_ptr() as usize % ALIGNMENT;
    let padding = if padding == ALIGNMENT { 0 } else { padding };
    for _ in 0..padding {
        recorder.buffer.push(0);
    }
    recorder.buffer.copy_within(0..original_length, padding);
    dcci_slice(&recorder.buffer);

    let ptr = recorder.buffer[padding..].as_ptr() as u32;

    let _old_record = DMA_RECORD_STORE
        .lock()
        .insert(recorder.name, (ptr, recorder.buffer, recorder.duration));

    #[cfg(has_drtio)]
    {
        if let Some((old_id, _v, _d)) = _old_record {
            remote_dma::erase(_aux_mutex, _routing_table, _timer, old_id).await;
        }
        remote_dma::add_traces(ptr, remote_traces);
    }

    ptr
}

pub async fn erase(name: String, _aux_mutex: &Rc<Mutex<bool>>, _routing_table: &RoutingTable, _timer: GlobalTimer) {
    let _entry = DMA_RECORD_STORE.lock().remove(&name);
    #[cfg(has_drtio)]
    if let Some((id, _v, _d)) = _entry {
        remote_dma::erase(_aux_mutex, _routing_table, _timer, id).await;
    }
}

pub async fn retrieve(name: String) -> Option<(i32, i64, bool)> {
    let (ptr, _v, duration) = DMA_RECORD_STORE.lock().get(&name)?.clone();
    #[cfg(has_drtio)]
    let uses_ddma = remote_dma::has_remote_traces(ptr).await;
    #[cfg(not(has_drtio))]
    let uses_ddma = false;
    Some((ptr as i32, duration, uses_ddma))
}
