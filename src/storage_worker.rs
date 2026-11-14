use std::io;
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use crate::storage::{self, Sample, Sampler, Volume};

pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const INVENTORY_INTERVAL: Duration = Duration::from_secs(5);

type Selection = Arc<[(String, u8)]>;

pub(crate) struct Observation {
    pub(crate) id: String,
    pub(crate) mask: u8,
    pub(crate) sample: Sample,
}

pub(crate) struct Batch {
    pub(crate) at: Instant,
    pub(crate) samples: Vec<Observation>,
}

pub(crate) struct Worker {
    state: Arc<Mutex<State>>,
    thread: Thread,
}

#[derive(Default)]
struct State {
    selection: Selection,
    generation: u64,
    inventory: Option<Vec<Volume>>,
    sample: Option<Batch>,
    stopped: bool,
}

impl Worker {
    pub(crate) fn start(consumer: Thread) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let worker_state = Arc::clone(&state);
        // A volume operation can block while the hardware wakes up. The dashboard
        // and shutdown both stay independent of the worker's completion.
        let worker = thread::Builder::new()
            .name("argos-armazenamento".into())
            .spawn(move || collect(&worker_state, &consumer))?;
        Ok(Self {
            state,
            thread: worker.thread().clone(),
        })
    }

    pub(crate) fn set_selection(&self, selection: Selection) {
        let mut state = self.state.lock().expect("coleta de armazenamento");
        if state.selection != selection {
            state.selection = selection;
            state.generation = state.generation.wrapping_add(1);
            state.sample = None;
            drop(state);
            self.thread.unpark();
        }
    }

    pub(crate) fn take_inventory(&self) -> Option<Vec<Volume>> {
        self.state.lock().ok()?.inventory.take()
    }

    pub(crate) fn take_sample(&self) -> Option<Batch> {
        self.state.lock().ok()?.sample.take()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
        }
        self.thread.unpark();
    }
}

fn collect(state: &Mutex<State>, consumer: &Thread) {
    let mut volumes = None;
    let mut selection_generation = None;
    let mut active: Vec<(Volume, u8)> = Vec::new();
    let mut sampler = Sampler::open(&[]);
    let mut next_inventory = Instant::now();
    let mut next_sample = next_inventory;

    loop {
        let mut inventory_changed = false;
        if Instant::now() >= next_inventory {
            if state.lock().expect("coleta de armazenamento").stopped {
                return;
            }
            if let Ok(inventory) = storage::volumes(volumes.as_deref().unwrap_or_default())
                && volumes.as_ref() != Some(&inventory)
            {
                let published = inventory.clone();
                let mut state = state.lock().expect("coleta de armazenamento");
                if state.stopped {
                    return;
                }
                state.inventory = Some(published);
                drop(state);
                volumes = Some(inventory);
                inventory_changed = true;
                consumer.unpark();
            }
            next_inventory = Instant::now() + INVENTORY_INTERVAL;
        }

        let (generation, selection) = {
            let state = state.lock().expect("coleta de armazenamento");
            if state.stopped {
                return;
            }
            let changed = selection_generation != Some(state.generation) || inventory_changed;
            (
                state.generation,
                changed.then(|| Arc::clone(&state.selection)),
            )
        };
        if let Some(selection) = selection {
            selection_generation = Some(generation);
            let selected = volumes
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|volume| {
                    let (_, mask) = selection.iter().find(|(id, _)| *id == volume.id)?;
                    (*mask != 0).then(|| (volume.clone(), *mask))
                })
                .collect::<Vec<_>>();
            if selected != active {
                if active.is_empty() {
                    next_sample = Instant::now();
                }
                active = selected;
                let selected = active
                    .iter()
                    .map(|(volume, mask)| (volume, *mask))
                    .collect::<Vec<_>>();
                sampler.set_volumes(&selected);
            }
        }

        if !active.is_empty() && Instant::now() >= next_sample {
            let values = sampler.sample();
            let at = Instant::now();
            next_sample = at + SAMPLE_INTERVAL;
            let samples = active
                .iter()
                .zip(values)
                .map(|((volume, mask), sample)| Observation {
                    id: volume.id.clone(),
                    mask: *mask,
                    sample,
                })
                .collect();
            let mut state = state.lock().expect("coleta de armazenamento");
            if !state.stopped && state.generation == generation {
                state.sample = Some(Batch { at, samples });
                drop(state);
                consumer.unpark();
            }
        }

        let deadline = if active.is_empty() {
            next_inventory
        } else {
            next_inventory.min(next_sample)
        };
        thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
    }
}
