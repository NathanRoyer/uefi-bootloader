use crate::{
    memory::{
        Frame, FrameRange, LegacyFrameAllocator, Mapper, Page, PageAllocator, PageRange,
        PhysicalAddress, PteFlags, UefiFrameAllocator, VirtualAddress, KERNEL_MEMORY,
    },
    util::calculate_pages,
};
use core::mem::MaybeUninit;
use goblin::elf64::program_header::ProgramHeader;
use uefi::{
    proto::{
        device_path::DevicePath,
        loaded_image::LoadedImage,
        media::{file::Directory, fs::SimpleFileSystem},
    },
    table::{
        boot::{AllocateType, MemoryMapSize, MemoryType},
        Boot, SystemTable,
    },
    Handle,
};

/// Bootloader context before extiting boot services.
pub(crate) struct BootContext {
    pub(crate) image_handle: Handle,
    pub(crate) system_table: SystemTable<Boot>,
    pub(crate) page_allocator: PageAllocator,
    pub(crate) mapper: Mapper,
}

impl BootContext {
    pub(crate) fn new(image_handle: Handle, system_table: SystemTable<Boot>) -> Self {
        let mut frame_allocator = UefiFrameAllocator {
            system_table: &system_table,
        };
        let mapper = Mapper::new(&mut frame_allocator);

        Self {
            image_handle,
            system_table,
            page_allocator: PageAllocator::new(),
            mapper,
        }
    }

    pub(crate) fn open_file_system_root(&self) -> Option<Directory> {
        let boot_services = self.system_table.boot_services();

        let loaded_image = boot_services
            .open_protocol_exclusive::<LoadedImage>(self.image_handle)
            .ok()?;
        let device_path = boot_services
            .open_protocol_exclusive::<DevicePath>(loaded_image.device())
            .ok()?;
        let device_handle = boot_services
            .locate_device_path::<SimpleFileSystem>(&mut &*device_path)
            .ok()?;
        boot_services
            .open_protocol_exclusive::<SimpleFileSystem>(device_handle)
            .ok()?
            .open_volume()
            .ok()
    }

    pub(crate) fn system_table(&self) -> &SystemTable<Boot> {
        &self.system_table
    }

    fn allocate_slice_inner<T>(
        &self,
        len: usize,
        allocate_type: AllocateType,
        memory_type: MemoryType,
    ) -> &'static mut [MaybeUninit<T>] {
        let bytes_len = core::mem::size_of::<T>() * len;
        let num_pages = calculate_pages(bytes_len);
        let pointer = self
            .system_table
            .boot_services()
            // TODO: Allocate pool?
            .allocate_pages(allocate_type, memory_type, num_pages)
            .expect("failed to allocate pages for slice") as *mut _;
        // SAFETY: We just allocated the memory at `pointer`.
        unsafe { core::ptr::write_bytes(pointer, 0, len) };
        // SAFETY: We just allocated the memory at `pointer`.
        let slice = unsafe { core::slice::from_raw_parts_mut(pointer, len) };
        slice
    }

    pub(crate) fn allocate_slice<T>(
        &self,
        len: usize,
        memory_type: MemoryType,
    ) -> &'static mut [MaybeUninit<T>] {
        self.allocate_slice_inner(len, AllocateType::AnyPages, memory_type)
    }

    pub(crate) fn allocate_byte_slice(&self, len: usize, ty: MemoryType) -> &'static mut [u8] {
        let slice = self.allocate_slice(len, ty);
        // SAFETY: allocate_slice zeroed the bytes so they are initialised.
        unsafe { MaybeUninit::slice_assume_init_mut(slice) }
    }

    pub(crate) fn map_segment(&mut self, segment: &ProgramHeader) -> &'static mut [u8] {
        // x86_64 .init section
        let slice = if segment.p_paddr == 0x10_0000 {
            let maybe_uninit_slice = self.allocate_slice_inner(
                segment.p_memsz as usize,
                AllocateType::Address(0x10_0000),
                KERNEL_MEMORY,
            );
            // SAFETY: allocate_slice_inner zeroed the bytes so they are initialised.
            unsafe { MaybeUninit::slice_assume_init_mut(maybe_uninit_slice) }
        } else {
            self.allocate_byte_slice(segment.p_memsz as usize, KERNEL_MEMORY)
        };

        self.page_allocator.mark_segment_as_used(segment);

        let virtual_start = VirtualAddress::new_canonical(segment.p_vaddr as usize);
        let virtual_end_inclusive = virtual_start + segment.p_memsz as usize - 1;

        let physical_start = PhysicalAddress::new_canonical(slice.as_ptr() as usize);
        let physical_end_inclusive = physical_start + segment.p_memsz as usize - 1;

        let pages = PageRange::new(
            Page::containing_address(virtual_start),
            Page::containing_address(virtual_end_inclusive),
        )
        .into_iter();
        let frames = FrameRange::new(
            Frame::containing_address(physical_start),
            Frame::containing_address(physical_end_inclusive),
        );

        let mut flags = PteFlags::new().present(true);

        // If the first bit isn't set
        if segment.p_flags & 0x1 == 0 {
            flags = flags.no_execute(true);
        }

        // If the second bit is set
        if segment.p_flags & 0x2 != 0 {
            flags = flags.writable(true);
        }

        for (page, frame) in pages.zip(frames) {
            self.mapper.map(
                page,
                frame,
                flags,
                &mut UefiFrameAllocator {
                    system_table: &self.system_table,
                },
            );
        }

        slice
    }

    pub(crate) fn exit_boot_services(self) -> RuntimeContext {
        let MemoryMapSize {
            entry_size,
            map_size,
        } = self.system_table.boot_services().memory_map_size();
        let predicted_map_size = map_size + (4 * entry_size);

        let memory_map_storage =
            self.allocate_byte_slice(predicted_map_size, MemoryType::LOADER_DATA);

        let (_, memory_map) = self
            .system_table
            .exit_boot_services(self.image_handle, memory_map_storage)
            .expect("failed to exit boot services");

        RuntimeContext {
            page_allocator: self.page_allocator,
            frame_allocator: LegacyFrameAllocator::new(memory_map),
            mapper: self.mapper,
        }
    }
}

/// Bootloader context after extiting boot services.
pub(crate) struct RuntimeContext {
    pub(crate) page_allocator: PageAllocator,
    pub(crate) frame_allocator: LegacyFrameAllocator,
    pub(crate) mapper: Mapper,
}

impl RuntimeContext {
    // TODO: This should take a shared reference to self.
    pub(crate) fn page_table(&mut self) -> Frame {
        self.mapper.frame()
    }
}
