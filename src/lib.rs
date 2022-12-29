#![allow(dead_code)]
#![feature(step_trait, abi_efiapi)]
#![no_std]

mod arch;
mod info;
mod load;
mod logger;
mod memory;

use crate::{
    info::{FrameBuffer, FrameBufferInfo},
    memory::Memory,
};
use core::{fmt::Write, ptr::NonNull};
use uefi::{
    prelude::entry,
    proto::console::gop::{GraphicsOutput, PixelFormat},
    table::{
        boot::{MemoryDescriptor, MemoryType},
        Boot, SystemTable,
    },
    Handle, Status,
};

static mut SYSTEM_TABLE: Option<NonNull<SystemTable<Boot>>> = None;

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    let system_table_pointer = NonNull::from(&mut system_table);
    unsafe { SYSTEM_TABLE = Some(system_table_pointer) };

    system_table
        .stdout()
        .clear()
        .expect("failed to clear stdout");

    let frame_buffer = get_frame_buffer(handle, &system_table);
    if let Some(frame_buffer) = frame_buffer {
        init_logger(&frame_buffer);
        log::info!("Using framebuffer at {:#x}", frame_buffer.start);
    }

    unsafe { SYSTEM_TABLE = None };

    let memory_map_buffer = {
        let memory_map_size = system_table.boot_services().memory_map_size().map_size
            + 8 * core::mem::size_of::<MemoryDescriptor>();
        let pointer = system_table
            .boot_services()
            .allocate_pool(MemoryType::LOADER_DATA, memory_map_size)
            .unwrap();
        unsafe { core::slice::from_raw_parts_mut(pointer, memory_map_size) }
    };
    let (_, memory_map) = system_table
        .boot_services()
        .memory_map(memory_map_buffer)
        .unwrap();

    let mut memory = Memory::new(memory_map);
    load::load_kernel(handle, &system_table, &mut memory);

    panic!();
}

fn get_frame_buffer(handle: Handle, system_table: &SystemTable<Boot>) -> Option<FrameBuffer> {
    let mut gop = system_table
        .boot_services()
        .open_protocol_exclusive::<GraphicsOutput>(handle)
        .ok()?;

    let mode_info = gop.current_mode_info();
    let mut frame_buffer = gop.frame_buffer();
    let info = FrameBufferInfo {
        len: frame_buffer.size(),
        width: mode_info.resolution().0,
        height: mode_info.resolution().1,
        pixel_format: match mode_info.pixel_format() {
            PixelFormat::Rgb => info::PixelFormat::Rgb,
            PixelFormat::Bgr => info::PixelFormat::Bgr,
            PixelFormat::Bitmask | PixelFormat::BltOnly => {
                panic!("Bitmask and BltOnly framebuffers are not supported")
            }
        },
        bytes_per_pixel: 4,
        stride: mode_info.stride(),
    };

    Some(FrameBuffer {
        start: frame_buffer.as_mut_ptr() as usize,
        info,
    })
}

fn init_logger(frame_buffer: &FrameBuffer) {
    let slice = unsafe {
        core::slice::from_raw_parts_mut(frame_buffer.start as *mut _, frame_buffer.info.len)
    };
    let logger =
        logger::LOGGER.call_once(move || logger::LockedLogger::new(slice, frame_buffer.info));
    log::set_logger(logger).expect("logger already set");
    log::set_max_level(log::LevelFilter::Trace);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if let Some(mut system_table_pointer) = unsafe { SYSTEM_TABLE } {
        let system_table = unsafe { system_table_pointer.as_mut() };
        let _ = writeln!(system_table.stdout(), "{}", info);
    }

    if let Some(logger) = logger::LOGGER.get() {
        unsafe { logger.force_unlock() };
    }
    log::error!("{}", info);

    loop {
        unsafe { core::arch::asm!("cli", "hlt") };
    }
}
