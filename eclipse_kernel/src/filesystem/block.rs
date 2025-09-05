//! Gestión de bloques para Eclipse OS

use crate::filesystem::BLOCK_SIZE;

// Dispositivo de bloques
pub struct BlockDevice {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub block_size: usize,
}

impl BlockDevice {
    pub fn new() -> Self {
        Self {
            total_blocks: 0,
            free_blocks: 0,
            block_size: BLOCK_SIZE,
        }
    }

    pub fn init(&mut self, total_blocks: u64) {
        self.total_blocks = total_blocks;
        self.free_blocks = total_blocks;
    }

    pub fn allocate_block(&mut self) -> Option<u64> {
        if self.free_blocks > 0 {
            self.free_blocks -= 1;
            Some(self.total_blocks - self.free_blocks - 1)
        } else {
            None
        }
    }

    pub fn free_block(&mut self, _block: u64) {
        if self.free_blocks < self.total_blocks {
            self.free_blocks += 1;
        }
    }
}

// Cache de bloques
pub struct BlockCache {
    pub blocks: [Option<[u8; BLOCK_SIZE]>; 32], // 32 bloques en cache
    pub block_numbers: [Option<u64>; 32],
}

impl BlockCache {
    pub fn new() -> Self {
        Self {
            blocks: [None; 32],
            block_numbers: [None; 32],
        }
    }

    pub fn get_block(&mut self, block_num: u64) -> Option<&mut [u8; BLOCK_SIZE]> {
        for i in 0..32 {
            if self.block_numbers[i] == Some(block_num) {
                return self.blocks[i].as_mut();
            }
        }
        None
    }

    pub fn put_block(&mut self, block_num: u64) -> &mut [u8; BLOCK_SIZE] {
        // Buscar slot libre
        for i in 0..32 {
            if self.blocks[i].is_none() {
                self.blocks[i] = Some([0; BLOCK_SIZE]);
                self.block_numbers[i] = Some(block_num);
                return self.blocks[i].as_mut().unwrap();
            }
        }
        
        // Si no hay slots libres, usar el primero (simplificado)
        self.blocks[0] = Some([0; BLOCK_SIZE]);
        self.block_numbers[0] = Some(block_num);
        self.blocks[0].as_mut().unwrap()
    }
}

// Instancia global del dispositivo de bloques
static mut BLOCK_DEVICE: Option<BlockDevice> = None;

pub fn init_block_device() -> Result<(), &'static str> {
    unsafe {
        BLOCK_DEVICE = Some(BlockDevice::new());
        if let Some(ref mut device) = BLOCK_DEVICE {
            device.init(1024); // 1024 bloques por defecto
        }
    }
    Ok(())
}

pub fn get_block_device() -> Option<&'static mut BlockDevice> {
    unsafe { BLOCK_DEVICE.as_mut() }
}
