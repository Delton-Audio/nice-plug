// DELTON FORK DELTA: au-1, AUv2 wrapper
//! Owned `AudioBufferList`s and channel storage. Everything here is allocated at Initialize; the
//! render path only fills in pointers.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::ptr::NonNull;

use super::sys::{AudioBuffer, AudioBufferList};
use crate::wrapper::util::buffer_management::ChannelPointers;

/// An AudioBufferList with room for `n` AudioBuffers, 8-byte aligned (`Vec<u64>` backing).
pub(crate) struct OwnedBufferList {
    storage: Vec<u64>,
    n: u32,
}

impl OwnedBufferList {
    /// A list with room for `n` buffers (at least one).
    pub fn new(n: u32) -> Self {
        let n = n.max(1);
        let bytes = offset_of!(AudioBufferList, mBuffers) + n as usize * size_of::<AudioBuffer>();
        Self {
            storage: vec![0u64; bytes.div_ceil(8)],
            n,
        }
    }

    /// The list, as the C struct. Valid for as long as this object is not moved or dropped.
    pub fn as_mut_ptr(&mut self) -> *mut AudioBufferList {
        self.storage.as_mut_ptr() as *mut AudioBufferList
    }

    /// Point each buffer at its channel: one channel per buffer, `frames` samples.
    pub fn prepare(&mut self, channels: &mut [Vec<f32>], frames: u32) {
        let count = self.n;
        let list = self.as_mut_ptr();
        // SAFETY: `list` points to `storage`, which has room for `count` buffers after the header.
        unsafe {
            (*list).mNumberBuffers = count;
            let first = (&raw mut (*list).mBuffers) as *mut AudioBuffer;
            for (i, channel) in channels.iter_mut().enumerate().take(count as usize) {
                first.add(i).write(AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: frames.saturating_mul(4),
                    mData: channel.as_mut_ptr() as *mut c_void,
                });
            }
        }
    }

    /// The data pointer of buffer `i`, read after the source was called (a source may point the
    /// buffers at its own memory).
    pub fn channel_ptr(&self, i: usize) -> *mut f32 {
        if i >= self.n as usize {
            return std::ptr::null_mut();
        }

        // SAFETY: `i` is in range, and the storage holds `n` buffers after the header. The list is
        // only read through a pointer derived from the storage's own pointer.
        unsafe {
            let list = self.storage.as_ptr() as *const AudioBufferList;
            let first = (&raw const (*list).mBuffers) as *const AudioBuffer;
            (*first.add(i)).mData as *mut f32
        }
    }
}

/// The wrapper's audio storage for one initialized configuration.
pub(crate) struct AuBuffers {
    /// One per input element: channels x max_frames f32, zeroed.
    input_channels: Vec<Vec<Vec<f32>>>,
    /// One per output element.
    output_channels: Vec<Vec<Vec<f32>>>,
    /// One AudioBufferList per input element, sized for its channel count.
    input_lists: Vec<OwnedBufferList>,
    /// The channel pointer array of each input element, as `BufferManager` wants it. These are
    /// `usize` instead of `*mut f32` so that this struct stays `Send + Sync`; the array is the same
    /// size and alignment as an array of pointers.
    input_pointers: Vec<Vec<usize>>,
    /// The same for each output element. Element 0 points at the host's buffers (or at our own when
    /// the host passed none) and is rewritten on every render.
    output_pointers: Vec<Vec<usize>>,
}

impl AuBuffers {
    /// Buffers for a layout: the channel count of every input element and output element.
    pub fn new(input_channels: &[u32], output_channels: &[u32], max_frames: usize) -> Self {
        let make_channels = |counts: &[u32]| -> Vec<Vec<Vec<f32>>> {
            counts
                .iter()
                .map(|&channels| vec![vec![0.0f32; max_frames]; channels as usize])
                .collect()
        };

        Self {
            input_channels: make_channels(input_channels),
            output_channels: make_channels(output_channels),
            input_lists: input_channels
                .iter()
                .map(|&channels| OwnedBufferList::new(channels))
                .collect(),
            input_pointers: input_channels
                .iter()
                .map(|&channels| vec![0usize; channels as usize])
                .collect(),
            output_pointers: output_channels
                .iter()
                .map(|&channels| vec![0usize; channels as usize])
                .collect(),
        }
    }

    /// The number of input elements.
    pub fn input_elements(&self) -> usize {
        self.input_lists.len()
    }

    /// Point input element `element`'s buffer list at our storage, ready to be handed to a source.
    pub fn prepare_input(&mut self, element: usize, frames: u32) -> *mut AudioBufferList {
        self.input_lists[element].prepare(&mut self.input_channels[element], frames);
        self.input_lists[element].as_mut_ptr()
    }

    /// Read the buffer pointers back out of input element `element`'s list after the pull. `pulled`
    /// is false when the source failed or does not exist: the element then reads as silence from our
    /// own storage.
    pub fn finish_input(&mut self, element: usize, frames: u32, pulled: bool) {
        let frames = frames as usize;
        for (channel, (storage, slot)) in self.input_channels[element]
            .iter_mut()
            .zip(self.input_pointers[element].iter_mut())
            .enumerate()
        {
            let from_source = if pulled {
                self.input_lists[element].channel_ptr(channel)
            } else {
                std::ptr::null_mut()
            };

            if from_source.is_null() {
                let end = frames.min(storage.len());
                storage[..end].fill(0.0);
                *slot = storage.as_mut_ptr() as usize;
            } else {
                *slot = from_source as usize;
            }
        }
    }

    /// Set up the output pointers from the host's buffer list. A null `mData` is set to our own
    /// channel buffer (the host reads the list back, which is legal in AU). Returns false if a
    /// buffer the host did supply is smaller than `frames` samples. Aux output elements always get
    /// our scratch buffers: au-1 does not support pulling them.
    ///
    /// # Safety
    ///
    /// `list` must point to a valid `AudioBufferList` whose `mNumberBuffers` equals the main output
    /// element's channel count.
    pub unsafe fn prepare_outputs(&mut self, list: *mut AudioBufferList, frames: u32) -> bool {
        let needed = frames.saturating_mul(4);
        if let Some(main) = self.output_channels.first_mut() {
            // SAFETY: The caller guarantees the list is valid for `main.len()` buffers.
            let first = unsafe { (&raw mut (*list).mBuffers) as *mut AudioBuffer };
            for (i, (storage, slot)) in main
                .iter_mut()
                .zip(self.output_pointers[0].iter_mut())
                .enumerate()
            {
                // SAFETY: See above.
                let buffer = unsafe { &mut *first.add(i) };
                if buffer.mData.is_null() {
                    buffer.mData = storage.as_mut_ptr() as *mut c_void;
                    buffer.mDataByteSize = needed;
                } else if buffer.mDataByteSize < needed {
                    return false;
                }
                *slot = buffer.mData as usize;
            }
        }

        for (channels, slots) in self
            .output_channels
            .iter_mut()
            .zip(self.output_pointers.iter_mut())
            .skip(1)
        {
            for (storage, slot) in channels.iter_mut().zip(slots.iter_mut()) {
                *slot = storage.as_mut_ptr() as usize;
            }
        }

        true
    }

    /// The channel pointers of input element `element`, as `BufferManager` wants them.
    pub fn input_channel_pointers(&mut self, element: usize) -> Option<ChannelPointers> {
        let slots = self.input_pointers.get_mut(element)?;
        Some(ChannelPointers {
            ptrs: NonNull::new(slots.as_mut_ptr() as *mut *mut f32)?,
            num_channels: slots.len(),
        })
    }

    /// The channel pointers of output element `element`, as `BufferManager` wants them.
    pub fn output_channel_pointers(&mut self, element: usize) -> Option<ChannelPointers> {
        let slots = self.output_pointers.get_mut(element)?;
        Some(ChannelPointers {
            ptrs: NonNull::new(slots.as_mut_ptr() as *mut *mut f32)?,
            num_channels: slots.len(),
        })
    }

    /// Fill the main output with silence.
    pub fn zero_main_output(&mut self, frames: u32) {
        let Some(outputs) = self.output_pointers.first() else {
            return;
        };

        for &output in outputs {
            if output != 0 {
                // SAFETY: Every pointer was set by `prepare_outputs` from a buffer that holds at
                // least `frames` samples.
                unsafe { std::ptr::write_bytes(output as *mut f32, 0, frames as usize) };
            }
        }
    }

    /// Copy the main input into the main output (bypass). Channels the input lacks are zeroed. The
    /// pointers may alias, so this uses `copy`.
    pub fn copy_main_input_to_output(&mut self, frames: u32) {
        let frames = frames as usize;
        let Some(outputs) = self.output_pointers.first() else {
            return;
        };
        let empty = Vec::new();
        let inputs = self.input_pointers.first().unwrap_or(&empty);

        for (channel, &output) in outputs.iter().enumerate() {
            let output = output as *mut f32;
            match inputs.get(channel) {
                // SAFETY: Every pointer was set from a buffer that holds at least `frames` samples:
                // our storage (max_frames), or a host buffer whose size was checked or that the
                // source filled for `frames` frames.
                Some(&input) if input != 0 => unsafe {
                    std::ptr::copy(input as *const f32, output, frames);
                },
                // SAFETY: See above.
                _ => unsafe { std::ptr::write_bytes(output, 0, frames) },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_list_covers_all_buffers() {
        let mut list = OwnedBufferList::new(3);
        let mut channels = vec![vec![0.0f32; 8]; 3];
        list.prepare(&mut channels, 8);
        let ptr = list.as_mut_ptr();
        assert_eq!(unsafe { (*ptr).mNumberBuffers }, 3);
        for (i, channel) in channels.iter_mut().enumerate() {
            assert_eq!(list.channel_ptr(i), channel.as_mut_ptr());
        }
        assert!(list.channel_ptr(3).is_null());
    }

    #[test]
    fn finish_input_falls_back_to_silence() {
        let mut buffers = AuBuffers::new(&[2], &[2], 16);
        buffers.input_channels[0][0].fill(1.0);
        let _ = buffers.prepare_input(0, 16);
        buffers.finish_input(0, 16, false);
        assert!(buffers.input_channels[0][0].iter().all(|&s| s == 0.0));
        assert_eq!(
            buffers.input_pointers[0][0],
            buffers.input_channels[0][0].as_ptr() as usize
        );
    }
}
