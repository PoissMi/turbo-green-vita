//! PS Vita hardware H.264 decoder (`sceVideodec`/`sceAvcdec`).
use super::memory::DecoderFrameBuffer;
use super::{DecoderConfig, YuvLayout};
use anyhow::{Result, bail};
use std::os::raw::c_void;
use vitasdk_sys::*;

// Exported by SceVideodecLowDelayUser (library NID 0x6354EEFF), not by SceVideodecUser which
// the SDK's libSceVideodec_stub.a imports from — that stub resolves to NULL on-device and the
// call crashes. build.rs assembles vita-stubs/SceVideodecLowDelayUser.S under a renamed symbol
// (same approach as vita-moonlight's custom stub).
unsafe extern "C" {
    #[link_name = "sceAvcdecSetLowDelayModeNongameapp_ld"]
    fn sceAvcdecSetLowDelayModeNongameapp(codec: SceVideodecType, mode: i32) -> i32;
}

const SCE_AVCDEC_DELAY_MODE_LOW: i32 = 0;

// Test: fewer ref frames = faster HW decode. 4 was chosen to prevent
// macroblock corruption in motion, but at 720p60 with 5 Mbps it may cost
// decode time. A/B this against d/a (decode time) and err.
const AVCDEC_NUM_REF_FRAMES: u32 = 2;
struct AvcdecLibrary {
    module_loaded: bool,
}

impl AvcdecLibrary {
    fn initialize(width: u32, height: u32) -> Result<Self> {
        let module_loaded = unsafe {
            let loaded_before = sceSysmoduleIsLoaded(SCE_SYSMODULE_AVCDEC);
            let ret = sceSysmoduleLoadModule(SCE_SYSMODULE_AVCDEC);
            if ret >= 0 {
                true
            } else if ret as u32 == SCE_SYSMODULE_ERROR_INVALID_VALUE {
                crate::elog!(
                    "sceSysmoduleLoadModule(SCE_SYSMODULE_AVCDEC=0x{SCE_SYSMODULE_AVCDEC:x}) returned {ret:#x}; continuing with SceVideodec imports; is_loaded_before={loaded_before:#x}",
                );
                false
            } else {
                bail!(
                    "sceSysmoduleLoadModule(SCE_SYSMODULE_AVCDEC=0x{SCE_SYSMODULE_AVCDEC:x}) failed: {ret:#x}; is_loaded_before={loaded_before:#x}",
                );
            }
        };

        // Shrink the decoder's internal reorder queue (vita-moonlight does the same); without
        // it, B-frame streams buffer several AUs before the first picture comes out.
        let low_delay_ret =
            unsafe { sceAvcdecSetLowDelayModeNongameapp(SCE_VIDEODEC_TYPE_HW_AVCDEC, SCE_AVCDEC_DELAY_MODE_LOW) };
        if low_delay_ret < 0 {
            crate::elog!("sceAvcdecSetLowDelayModeNongameapp failed: {low_delay_ret:#x}; continuing");
        }

        let init_info = SceVideodecQueryInitInfoHwAvcdec {
            size: size_of::<SceVideodecQueryInitInfoHwAvcdec>() as u32,
            horizontal: width,
            vertical: height,
            numOfRefFrames: AVCDEC_NUM_REF_FRAMES,
            numOfStreams: 1,
        };
        let ret = unsafe { sceVideodecInitLibrary(SCE_VIDEODEC_TYPE_HW_AVCDEC, &init_info) };
        if ret < 0 {
            if module_loaded {
                unsafe {
                    sceSysmoduleUnloadModule(SCE_SYSMODULE_AVCDEC);
                }
            }
            bail!("sceVideodecInitLibrary failed: {ret:#x}");
        }

        Ok(Self { module_loaded })
    }
}

impl Drop for AvcdecLibrary {
    fn drop(&mut self) {
        unsafe {
            sceVideodecTermLibrary(SCE_VIDEODEC_TYPE_HW_AVCDEC);
            if self.module_loaded {
                sceSysmoduleUnloadModule(SCE_SYSMODULE_AVCDEC);
            }
        }
    }
}

struct AvcdecDecoder(SceAvcdecCtrl);

impl Drop for AvcdecDecoder {
    fn drop(&mut self) {
        unsafe {
            sceAvcdecDeleteDecoder(&mut self.0);
        }
    }
}

pub struct HwVideoDecoder {
    decoder: AvcdecDecoder,
    _library: AvcdecLibrary,
    _frame_memory: DecoderFrameBuffer,
    pub(super) layout: YuvLayout,
}

impl HwVideoDecoder {
    pub(super) fn uses_phycont(&self) -> bool {
        self._frame_memory.is_phycont()
    }

    pub fn new(config: DecoderConfig) -> Result<Self> {
        unsafe {
            let library = AvcdecLibrary::initialize(config.decode_width, config.decode_height)?;

            let query = SceAvcdecQueryDecoderInfo {
                horizontal: config.decode_width,
                vertical: config.decode_height,
                numOfRefFrames: AVCDEC_NUM_REF_FRAMES,
            };
            let mut decoder_info = SceAvcdecDecoderInfo { frameMemSize: 0 };
            let ret = sceAvcdecQueryDecoderMemSize(
                SCE_VIDEODEC_TYPE_HW_AVCDEC,
                &query,
                &mut decoder_info,
            );
            if ret < 0 {
                bail!("sceAvcdecQueryDecoderMemSize failed: {ret:#x}");
            }
            let frame_memory = DecoderFrameBuffer::acquire(decoder_info.frameMemSize)?;
            let mut decoder_control = SceAvcdecCtrl {
                handle: 0,
                frameBuf: SceAvcdecBuf {
                    pBuf: frame_memory.as_ptr(),
                    size: decoder_info.frameMemSize,
                },
            };
            let ret =
                sceAvcdecCreateDecoder(SCE_VIDEODEC_TYPE_HW_AVCDEC, &mut decoder_control, &query);
            if ret < 0 {
                bail!("sceAvcdecCreateDecoder failed: {ret:#x}");
            }
            let decoder = AvcdecDecoder(decoder_control);

            Ok(Self {
                decoder,
                _library: library,
                _frame_memory: frame_memory,
                layout: YuvLayout::new(config.decode_width, config.decode_height)?,
            })
        }
    }

    /// Decodes one Access Unit. Returns `false` if the hardware buffered it without producing a picture yet.
    pub fn decode(
        &mut self,
        access_unit: &[u8],
        output_ptr: *mut u8,
    ) -> Result<Option<[u32; 4]>> {
        unsafe {
            let au = SceAvcdecAu {
                pts: SceVideodecTimeStamp {
                    upper: 0xFFFFFFFF,
                    lower: 0xFFFFFFFF,
                },
                dts: SceVideodecTimeStamp {
                    upper: 0xFFFFFFFF,
                    lower: 0xFFFFFFFF,
                },
                es: SceAvcdecBuf {
                    pBuf: access_unit.as_ptr() as *mut c_void,
                    size: access_unit.len() as u32,
                },
            };

            let layout = self.layout;
            let uv = output_ptr.add(layout.uv_offset as usize);
            let mut picture = SceAvcdecPicture {
                size: size_of::<SceAvcdecPicture>() as u32,
                frame: SceAvcdecFrame {
                    pixelType: SCE_AVCDEC_PIXELFORMAT_YUV420_PACKED_RASTER as u32,
                    framePitch: layout.width,
                    frameWidth: layout.width,
                    frameHeight: layout.height,
                    horizontalSize: layout.width,
                    verticalSize: layout.height,
                    frameCropLeftOffset: 0,
                    frameCropRightOffset: 0,
                    frameCropTopOffset: 0,
                    frameCropBottomOffset: 0,
                    opt: SceAvcdecFrameOption { reserved: [0; 16] },
                    pPicture: [output_ptr.cast(), uv.cast()],
                },
                info: std::mem::zeroed(),
            };
            let mut picture_ptr: *mut SceAvcdecPicture = &mut picture;
            let mut array_picture = SceAvcdecArrayPicture {
                numOfOutput: 0,
                numOfElm: 1,
                pPicture: &mut picture_ptr,
            };

            let ret = sceAvcdecDecode(&self.decoder.0, &au, &mut array_picture);
            if ret < 0 {
                bail!("sceAvcdecDecode failed: {ret:#x}");
            }
            if array_picture.numOfOutput == 0 {
                return Ok(None);
            }
            let frame = picture.frame;
            anyhow::ensure!(frame.framePitch == layout.width && frame.pPicture == [output_ptr.cast(), uv.cast()], "unexpected AVCDEC YUV layout");
            let (x, y) = (frame.frameCropLeftOffset, frame.frameCropTopOffset);
            let w = frame.horizontalSize.min(layout.width).saturating_sub(x.saturating_add(frame.frameCropRightOffset));
            let h = frame.verticalSize.min(layout.height).saturating_sub(y.saturating_add(frame.frameCropBottomOffset));
            anyhow::ensure!(w > 0 && h > 0, "invalid AVCDEC picture crop");
            Ok(Some([x, y, w, h]))
        }
    }
}

// SAFETY: the CDRAM blocks and decoder handle have no thread affinity in the underlying SCE API -
// this is only ever moved once (into `VideoDecodeWorker`'s thread), never accessed concurrently.
unsafe impl Send for HwVideoDecoder {}
