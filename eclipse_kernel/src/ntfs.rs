//! ReactOS Rust Kernel - NTFS File System
//!
//! Implementación del sistema de archivos NTFS.

use core::arch::asm;

/// Boot Sector de NTFS
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct NtfsBootSector {
    pub jump_instruction: [u8; 3],
    pub oem_name: [u8; 8],
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub media_type: u8,
    pub sectors_per_track: u16,
    pub number_of_heads: u16,
    pub hidden_sectors: u32,
    pub total_sectors: u64,
    pub mft_cluster: u64,
    pub mft_mirror_cluster: u64,
    pub clusters_per_mft_record: u8,
    pub clusters_per_index_block: u8,
    pub volume_serial: u64,
    pub checksum: u32,
    pub bootstrap_code: [u8; 426],
    pub boot_signature: u16,
}

/// Tipo de registro MFT
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MftRecordType {
    File = 0x01,
    Directory = 0x02,
    Volume = 0x03,
    AttributeDef = 0x04,
    RootIndex = 0x05,
    Bitmap = 0x06,
    Boot = 0x07,
    BadCluster = 0x08,
    Quota = 0x09,
    UpCase = 0x0A,
    Extend = 0x0B,
    Reserved = 0x0C,
    LogFile = 0x0D,
    Dsd = 0x0E,
    Reserved2 = 0x0F,
    Reserved3 = 0x10,
    Reserved4 = 0x11,
    Reserved5 = 0x12,
    Reserved6 = 0x13,
    Reserved7 = 0x14,
    Reserved8 = 0x15,
    Reserved9 = 0x16,
    Reserved10 = 0x17,
    Reserved11 = 0x18,
    Reserved12 = 0x19,
    Reserved13 = 0x1A,
    Reserved14 = 0x1B,
    Reserved15 = 0x1C,
    Reserved16 = 0x1D,
    Reserved17 = 0x1E,
    Reserved18 = 0x1F,
    Reserved19 = 0x20,
    Reserved20 = 0x21,
    Reserved21 = 0x22,
    Reserved22 = 0x23,
    Reserved23 = 0x24,
    Reserved24 = 0x25,
    Reserved25 = 0x26,
    Reserved26 = 0x27,
    Reserved27 = 0x28,
    Reserved28 = 0x29,
    Reserved29 = 0x2A,
    Reserved30 = 0x2B,
    Reserved31 = 0x2C,
    Reserved32 = 0x2D,
    Reserved33 = 0x2E,
    Reserved34 = 0x2F,
    Reserved35 = 0x30,
    Reserved36 = 0x31,
    Reserved37 = 0x32,
    Reserved38 = 0x33,
    Reserved39 = 0x34,
    Reserved40 = 0x35,
    Reserved41 = 0x36,
    Reserved42 = 0x37,
    Reserved43 = 0x38,
    Reserved44 = 0x39,
    Reserved45 = 0x3A,
    Reserved46 = 0x3B,
    Reserved47 = 0x3C,
    Reserved48 = 0x3D,
    Reserved49 = 0x3E,
    Reserved50 = 0x3F,
    Reserved51 = 0x40,
    Reserved52 = 0x41,
    Reserved53 = 0x42,
    Reserved54 = 0x43,
    Reserved55 = 0x44,
    Reserved56 = 0x45,
    Reserved57 = 0x46,
    Reserved58 = 0x47,
    Reserved59 = 0x48,
    Reserved60 = 0x49,
    Reserved61 = 0x4A,
    Reserved62 = 0x4B,
    Reserved63 = 0x4C,
    Reserved64 = 0x4D,
    Reserved65 = 0x4E,
    Reserved66 = 0x4F,
    Reserved67 = 0x50,
    Reserved68 = 0x51,
    Reserved69 = 0x52,
    Reserved70 = 0x53,
    Reserved71 = 0x54,
    Reserved72 = 0x55,
    Reserved73 = 0x56,
    Reserved74 = 0x57,
    Reserved75 = 0x58,
    Reserved76 = 0x59,
    Reserved77 = 0x5A,
    Reserved78 = 0x5B,
    Reserved79 = 0x5C,
    Reserved80 = 0x5D,
    Reserved81 = 0x5E,
    Reserved82 = 0x5F,
    Reserved83 = 0x60,
    Reserved84 = 0x61,
    Reserved85 = 0x62,
    Reserved86 = 0x63,
    Reserved87 = 0x64,
    Reserved88 = 0x65,
    Reserved89 = 0x66,
    Reserved90 = 0x67,
    Reserved91 = 0x68,
    Reserved92 = 0x69,
    Reserved93 = 0x6A,
    Reserved94 = 0x6B,
    Reserved95 = 0x6C,
    Reserved96 = 0x6D,
    Reserved97 = 0x6E,
    Reserved98 = 0x6F,
    Reserved99 = 0x70,
    Reserved100 = 0x71,
    Reserved101 = 0x72,
    Reserved102 = 0x73,
    Reserved103 = 0x74,
    Reserved104 = 0x75,
    Reserved105 = 0x76,
    Reserved106 = 0x77,
    Reserved107 = 0x78,
    Reserved108 = 0x79,
    Reserved109 = 0x7A,
    Reserved110 = 0x7B,
    Reserved111 = 0x7C,
    Reserved112 = 0x7D,
    Reserved113 = 0x7E,
    Reserved114 = 0x7F,
    Reserved115 = 0x80,
    Reserved116 = 0x81,
    Reserved117 = 0x82,
    Reserved118 = 0x83,
    Reserved119 = 0x84,
    Reserved120 = 0x85,
    Reserved121 = 0x86,
    Reserved122 = 0x87,
    Reserved123 = 0x88,
    Reserved124 = 0x89,
    Reserved125 = 0x8A,
    Reserved126 = 0x8B,
    Reserved127 = 0x8C,
    Reserved128 = 0x8D,
    Reserved129 = 0x8E,
    Reserved130 = 0x8F,
    Reserved131 = 0x90,
    Reserved132 = 0x91,
    Reserved133 = 0x92,
    Reserved134 = 0x93,
    Reserved135 = 0x94,
    Reserved136 = 0x95,
    Reserved137 = 0x96,
    Reserved138 = 0x97,
    Reserved139 = 0x98,
    Reserved140 = 0x99,
    Reserved141 = 0x9A,
    Reserved142 = 0x9B,
    Reserved143 = 0x9C,
    Reserved144 = 0x9D,
    Reserved145 = 0x9E,
    Reserved146 = 0x9F,
    Reserved147 = 0xA0,
    Reserved148 = 0xA1,
    Reserved149 = 0xA2,
    Reserved150 = 0xA3,
    Reserved151 = 0xA4,
    Reserved152 = 0xA5,
    Reserved153 = 0xA6,
    Reserved154 = 0xA7,
    Reserved155 = 0xA8,
    Reserved156 = 0xA9,
    Reserved157 = 0xAA,
    Reserved158 = 0xAB,
    Reserved159 = 0xAC,
    Reserved160 = 0xAD,
    Reserved161 = 0xAE,
    Reserved162 = 0xAF,
    Reserved163 = 0xB0,
    Reserved164 = 0xB1,
    Reserved165 = 0xB2,
    Reserved166 = 0xB3,
    Reserved167 = 0xB4,
    Reserved168 = 0xB5,
    Reserved169 = 0xB6,
    Reserved170 = 0xB7,
    Reserved171 = 0xB8,
    Reserved172 = 0xB9,
    Reserved173 = 0xBA,
    Reserved174 = 0xBB,
    Reserved175 = 0xBC,
    Reserved176 = 0xBD,
    Reserved177 = 0xBE,
    Reserved178 = 0xBF,
    Reserved179 = 0xC0,
    Reserved180 = 0xC1,
    Reserved181 = 0xC2,
    Reserved182 = 0xC3,
    Reserved183 = 0xC4,
    Reserved184 = 0xC5,
    Reserved185 = 0xC6,
    Reserved186 = 0xC7,
    Reserved187 = 0xC8,
    Reserved188 = 0xC9,
    Reserved189 = 0xCA,
    Reserved190 = 0xCB,
    Reserved191 = 0xCC,
    Reserved192 = 0xCD,
    Reserved193 = 0xCE,
    Reserved195 = 0xD0,
    Reserved196 = 0xD1,
    Reserved197 = 0xD2,
    Reserved198 = 0xD3,
    Reserved199 = 0xD4,
    Reserved200 = 0xD5,
    Reserved201 = 0xD6,
    Reserved202 = 0xD7,
    Reserved203 = 0xD8,
    Reserved204 = 0xD9,
    Reserved205 = 0xDA,
    Reserved206 = 0xDB,
    Reserved207 = 0xDC,
    Reserved208 = 0xDD,
    Reserved209 = 0xDE,
    Reserved210 = 0xDF,
    Reserved211 = 0xE0,
    Reserved212 = 0xE1,
    Reserved213 = 0xE2,
    Reserved214 = 0xE3,
    Reserved215 = 0xE4,
    Reserved216 = 0xE5,
    Reserved217 = 0xE6,
    Reserved218 = 0xE7,
    Reserved219 = 0xE8,
    Reserved220 = 0xE9,
    Reserved221 = 0xEA,
    Reserved222 = 0xEB,
    Reserved223 = 0xEC,
    Reserved224 = 0xED,
    Reserved225 = 0xEE,
    Reserved226 = 0xEF,
    Reserved227 = 0xF0,
    Reserved228 = 0xF1,
    Reserved229 = 0xF2,
    Reserved230 = 0xF3,
    Reserved231 = 0xF4,
    Reserved232 = 0xF5,
    Reserved233 = 0xF6,
    Reserved234 = 0xF7,
    Reserved235 = 0xF8,
    Reserved236 = 0xF9,
    Reserved237 = 0xFA,
    Reserved238 = 0xFB,
    Reserved239 = 0xFC,
    Reserved240 = 0xFD,
    Reserved241 = 0xFE,
    Reserved242 = 0xFF,
}

/// Cabecera de registro MFT
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct MftRecordHeader {
    pub signature: [u8; 4],
    pub update_sequence_offset: u16,
    pub update_sequence_size: u16,
    pub log_file_sequence: u64,
    pub sequence_number: u16,
    pub link_count: u16,
    pub attributes_offset: u16,
    pub flags: u16,
    pub bytes_in_use: u32,
    pub bytes_allocated: u32,
    pub base_record: u64,
    pub next_attribute_id: u16,
    pub record_number: u16,
    pub mft_record_number: u32,
}

/// Tipo de atributo NTFS
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NtfsAttributeType {
    StandardInformation = 0x10,
    AttributeList = 0x20,
    FileName = 0x30,
    ObjectId = 0x40,
    SecurityDescriptor = 0x50,
    VolumeName = 0x60,
    VolumeInformation = 0x70,
    Data = 0x80,
    IndexRoot = 0x90,
    IndexAllocation = 0xA0,
    Bitmap = 0xB0,
    ReparsePoint = 0xC0,
    EAInformation = 0xD0,
    EA = 0xE0,
    PropertySet = 0xF0,
    LoggedUtilityStream = 0x100,
    EndOfAttributes = 0xFFFFFFFF,
}

/// Cabecera de atributo NTFS
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct NtfsAttributeHeader {
    pub attribute_type: u32,
    pub length: u32,
    pub non_resident: u8,
    pub name_length: u8,
    pub name_offset: u16,
    pub flags: u16,
    pub attribute_id: u16,
}

/// Atributo residente
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct NtfsResidentAttribute {
    pub header: NtfsAttributeHeader,
    pub content_size: u32,
    pub content_offset: u16,
    pub indexed: u8,
    pub padding: u8,
}

/// Atributo no residente
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct NtfsNonResidentAttribute {
    pub header: NtfsAttributeHeader,
    pub starting_vcn: u64,
    pub ending_vcn: u64,
    pub runlist_offset: u16,
    pub compression_unit: u16,
    pub allocated_size: u64,
    pub actual_size: u64,
    pub initialized_size: u64,
}

/// Información estándar de archivo
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct NtfsStandardInformation {
    pub creation_time: u64,
    pub file_change_time: u64,
    pub mft_change_time: u64,
    pub last_access_time: u64,
    pub file_attributes: u32,
    pub max_versions: u32,
    pub version_number: u32,
    pub class_id: u32,
    pub owner_id: u32,
    pub security_id: u32,
    pub quota_charged: u64,
    pub usn: u64,
}

/// Nombre de archivo NTFS
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct NtfsFileName {
    pub parent_directory: u64,
    pub creation_time: u64,
    pub file_change_time: u64,
    pub mft_change_time: u64,
    pub last_access_time: u64,
    pub allocated_size: u64,
    pub actual_size: u64,
    pub file_attributes: u32,
    pub reparse_point: u32,
    pub name_length: u8,
    pub name_type: u8,
    pub name: [u16; 255], // Nombre Unicode
}

/// Driver NTFS
pub struct NtfsDriver {
    pub boot_sector: NtfsBootSector,
    pub mft_start_cluster: u64,
    pub mft_record_size: u32,
    pub cluster_size: u32,
    pub total_clusters: u64,
    pub device_id: u32,
    pub mft_cache: [Option<MftRecordHeader>; 256],
    pub cache_hits: u32,
    pub cache_misses: u32,
}

impl NtfsDriver {
    /// Crear un nuevo driver NTFS
    pub fn new(device_id: u32) -> Self {
        Self {
            boot_sector: NtfsBootSector {
                jump_instruction: [0xEB, 0x52, 0x90],
                oem_name: *b"NTFS    ",
                bytes_per_sector: 512,
                sectors_per_cluster: 8,
                reserved_sectors: 0,
                media_type: 0xF8,
                sectors_per_track: 63,
                number_of_heads: 255,
                hidden_sectors: 0,
                total_sectors: 0,
                mft_cluster: 0,
                mft_mirror_cluster: 0,
                clusters_per_mft_record: 0,
                clusters_per_index_block: 0,
                volume_serial: 0,
                checksum: 0,
                bootstrap_code: [0; 426],
                boot_signature: 0xAA55,
            },
            mft_start_cluster: 0,
            mft_record_size: 1024,
            cluster_size: 0,
            total_clusters: 0,
            device_id,
            mft_cache: [None; 256],
            cache_hits: 0,
            cache_misses: 0,
        }
    }
    
    /// Inicializar driver NTFS
    pub fn init(&mut self) -> Result<(), &'static str> {
        // Leer boot sector
        self.read_boot_sector()?;
        
        // Calcular parámetros
        self.cluster_size = self.boot_sector.bytes_per_sector as u32 * self.boot_sector.sectors_per_cluster as u32;
        self.total_clusters = self.boot_sector.total_sectors / self.boot_sector.sectors_per_cluster as u64;
        self.mft_start_cluster = self.boot_sector.mft_cluster;
        
        // Calcular tamaño de registro MFT
        if (self.boot_sector.clusters_per_mft_record as i8) < 0 {
            self.mft_record_size = 1 << (-(self.boot_sector.clusters_per_mft_record as i8) as u32);
        } else {
            self.mft_record_size = self.boot_sector.clusters_per_mft_record as u32 * self.cluster_size;
        }
        
        // Inicializar caché MFT
        for i in 0..256 {
            self.mft_cache[i] = None;
        }
        
        Ok(())
    }
    
    /// Leer boot sector
    fn read_boot_sector(&mut self) -> Result<(), &'static str> {
        // TODO: Implementar lectura del boot sector desde el dispositivo
        // Por ahora, usar valores por defecto
        self.boot_sector.bytes_per_sector = 512;
        self.boot_sector.sectors_per_cluster = 8;
        self.boot_sector.total_sectors = 1048576; // 512MB
        self.boot_sector.mft_cluster = 4;
        self.boot_sector.mft_mirror_cluster = 5;
        self.boot_sector.clusters_per_mft_record = 0; // 1KB por registro
        self.boot_sector.clusters_per_index_block = 1; // 4KB por bloque
        self.boot_sector.volume_serial = 0x123456789ABCDEF0;
        
        Ok(())
    }
    
    /// Leer registro MFT
    pub fn read_mft_record(&mut self, record_number: u64) -> Result<MftRecordHeader, &'static str> {
        // Verificar caché
        let cache_index = (record_number % 256) as usize;
        if let Some(cached_record) = self.mft_cache[cache_index] {
            if cached_record.mft_record_number == record_number as u32 {
                self.cache_hits += 1;
                return Ok(cached_record);
            }
        }
        
        // Leer desde disco
        let cluster_offset = record_number * (self.mft_record_size as u64 / self.cluster_size as u64);
        let sector_offset = cluster_offset * (self.cluster_size as u64 / 512);
        
        let mut buffer = [0u8; 1024];
        self.read_sectors(sector_offset, 2, &mut buffer)?;
        
        let mut header = MftRecordHeader {
            signature: [0; 4],
            update_sequence_offset: 0,
            update_sequence_size: 0,
            log_file_sequence: 0,
            sequence_number: 0,
            link_count: 0,
            attributes_offset: 0,
            flags: 0,
            bytes_in_use: 0,
            bytes_allocated: 0,
            base_record: 0,
            next_attribute_id: 0,
            record_number: 0,
            mft_record_number: 0,
        };
        
        // Copiar datos del buffer
        header.signature.copy_from_slice(&buffer[0..4]);
        header.update_sequence_offset = u16::from_le_bytes([buffer[4], buffer[5]]);
        header.update_sequence_size = u16::from_le_bytes([buffer[6], buffer[7]]);
        header.log_file_sequence = u64::from_le_bytes([
            buffer[8], buffer[9], buffer[10], buffer[11],
            buffer[12], buffer[13], buffer[14], buffer[15],
        ]);
        header.sequence_number = u16::from_le_bytes([buffer[16], buffer[17]]);
        header.link_count = u16::from_le_bytes([buffer[18], buffer[19]]);
        header.attributes_offset = u16::from_le_bytes([buffer[20], buffer[21]]);
        header.flags = u16::from_le_bytes([buffer[22], buffer[23]]);
        header.bytes_in_use = u32::from_le_bytes([buffer[24], buffer[25], buffer[26], buffer[27]]);
        header.bytes_allocated = u32::from_le_bytes([buffer[28], buffer[29], buffer[30], buffer[31]]);
        header.base_record = u64::from_le_bytes([
            buffer[32], buffer[33], buffer[34], buffer[35],
            buffer[36], buffer[37], buffer[38], buffer[39],
        ]);
        header.next_attribute_id = u16::from_le_bytes([buffer[40], buffer[41]]);
        header.record_number = u16::from_le_bytes([buffer[42], buffer[43]]);
        header.mft_record_number = u32::from_le_bytes([buffer[44], buffer[45], buffer[46], buffer[47]]);
        
        // Verificar firma
        if &header.signature != b"FILE" {
            return Err("Invalid MFT record signature");
        }
        
        // Actualizar caché
        self.mft_cache[cache_index] = Some(header);
        self.cache_misses += 1;
        
        Ok(header)
    }
    
    /// Leer sectores del dispositivo
    fn read_sectors(&self, start_sector: u64, count: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        // TODO: Implementar lectura real del dispositivo
        // Por ahora, llenar con datos simulados
        let bytes_to_read = core::cmp::min(buffer.len(), (count * 512) as usize);
        for i in 0..bytes_to_read {
            buffer[i] = (start_sector as u8).wrapping_add(i as u8);
        }
        Ok(())
    }
    
    /// Escribir sectores al dispositivo
    fn write_sectors(&self, _start_sector: u64, _count: u32, _buffer: &[u8]) -> Result<(), &'static str> {
        // TODO: Implementar escritura real al dispositivo
        // Por ahora, simular escritura exitosa
        Ok(())
    }
    
    /// Leer atributo de archivo
    pub fn read_attribute<'a>(&self, record_buffer: &'a [u8], attribute_type: NtfsAttributeType) -> Result<&'a [u8], &'static str> {
        let mut offset = 0;
        
        while offset < record_buffer.len() {
            let attr_header = &record_buffer[offset..offset + 16];
            let attr_type = u32::from_le_bytes([attr_header[0], attr_header[1], attr_header[2], attr_header[3]]);
            let attr_length = u32::from_le_bytes([attr_header[4], attr_header[5], attr_header[6], attr_header[7]]);
            
            if attr_type == attribute_type as u32 {
                return Ok(&record_buffer[offset..offset + attr_length as usize]);
            }
            
            if attr_type == NtfsAttributeType::EndOfAttributes as u32 {
                break;
            }
            
            offset += attr_length as usize;
        }
        
        Err("Attribute not found")
    }
    
    /// Leer nombre de archivo
    pub fn read_filename(&self, record_buffer: &[u8]) -> Result<[u8; 256], &'static str> {
        let attr_data = self.read_attribute(record_buffer, NtfsAttributeType::FileName)?;
        
        if attr_data.len() < 66 {
            return Err("Invalid filename attribute");
        }
        
        let name_length = attr_data[64] as usize;
        if name_length == 0 || name_length > 255 {
            return Err("Invalid filename length");
        }
        
        let name_bytes = &attr_data[66..66 + name_length * 2];
        let mut filename = [0u8; 256];
        
        // Convertir UTF-16 a ASCII (simplificado)
        let mut pos = 0;
        for i in (0..name_length * 2).step_by(2) {
            if i + 1 < name_bytes.len() && pos < 255 {
                let char_code = u16::from_le_bytes([name_bytes[i], name_bytes[i + 1]]);
                if char_code != 0 && char_code < 256 {
                    filename[pos] = char_code as u8;
                    pos += 1;
                }
            }
        }
        
        Ok(filename)
    }
    
    /// Leer información estándar de archivo
    pub fn read_standard_information(&self, record_buffer: &[u8]) -> Result<NtfsStandardInformation, &'static str> {
        let attr_data = self.read_attribute(record_buffer, NtfsAttributeType::StandardInformation)?;
        
        if attr_data.len() < 72 {
            return Err("Invalid standard information attribute");
        }
        
        let mut info = NtfsStandardInformation {
            creation_time: 0,
            file_change_time: 0,
            mft_change_time: 0,
            last_access_time: 0,
            file_attributes: 0,
            max_versions: 0,
            version_number: 0,
            class_id: 0,
            owner_id: 0,
            security_id: 0,
            quota_charged: 0,
            usn: 0,
        };
        
        // Copiar datos del buffer
        info.creation_time = u64::from_le_bytes([
            attr_data[16], attr_data[17], attr_data[18], attr_data[19],
            attr_data[20], attr_data[21], attr_data[22], attr_data[23],
        ]);
        info.file_change_time = u64::from_le_bytes([
            attr_data[24], attr_data[25], attr_data[26], attr_data[27],
            attr_data[28], attr_data[29], attr_data[30], attr_data[31],
        ]);
        info.mft_change_time = u64::from_le_bytes([
            attr_data[32], attr_data[33], attr_data[34], attr_data[35],
            attr_data[36], attr_data[37], attr_data[38], attr_data[39],
        ]);
        info.last_access_time = u64::from_le_bytes([
            attr_data[40], attr_data[41], attr_data[42], attr_data[43],
            attr_data[44], attr_data[45], attr_data[46], attr_data[47],
        ]);
        info.file_attributes = u32::from_le_bytes([attr_data[48], attr_data[49], attr_data[50], attr_data[51]]);
        info.max_versions = u32::from_le_bytes([attr_data[52], attr_data[53], attr_data[54], attr_data[55]]);
        info.version_number = u32::from_le_bytes([attr_data[56], attr_data[57], attr_data[58], attr_data[59]]);
        info.class_id = u32::from_le_bytes([attr_data[60], attr_data[61], attr_data[62], attr_data[63]]);
        info.owner_id = u32::from_le_bytes([attr_data[64], attr_data[65], attr_data[66], attr_data[67]]);
        info.security_id = u32::from_le_bytes([attr_data[68], attr_data[69], attr_data[70], attr_data[71]]);
        
        Ok(info)
    }
    
    /// Leer datos de archivo
    pub fn read_file_data(&self, record_buffer: &[u8], offset: u64, size: usize, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let attr_data = self.read_attribute(record_buffer, NtfsAttributeType::Data)?;
        
        if attr_data.len() < 16 {
            return Err("Invalid data attribute");
        }
        
        let non_resident = attr_data[8];
        if non_resident == 0 {
            // Atributo residente
            let content_size = u32::from_le_bytes([attr_data[16], attr_data[17], attr_data[18], attr_data[19]]);
            let content_offset = u16::from_le_bytes([attr_data[20], attr_data[21]]) as usize;
            
            if offset >= content_size as u64 {
                return Ok(0);
            }
            
            let start = content_offset + offset as usize;
            let end = core::cmp::min(start + size, content_offset + content_size as usize);
            
            if start >= attr_data.len() || end > attr_data.len() {
                return Err("Data out of bounds");
            }
            
            let bytes_to_copy = core::cmp::min(end - start, buffer.len());
            buffer[..bytes_to_copy].copy_from_slice(&attr_data[start..start + bytes_to_copy]);
            Ok(bytes_to_copy)
        } else {
            // Atributo no residente
            // TODO: Implementar lectura de datos no residentes
            Err("Non-resident data not implemented")
        }
    }
    
    /// Buscar archivo en directorio
    pub fn find_file(&mut self, directory_record: u64, _filename: &str) -> Result<u64, &'static str> {
        let mut buffer = [0u8; 1024];
        let _record = self.read_mft_record(directory_record)?;
        
        // Leer registro completo
        let cluster_offset = directory_record * (self.mft_record_size as u64 / self.cluster_size as u64);
        let sector_offset = cluster_offset * (self.cluster_size as u64 / 512);
        self.read_sectors(sector_offset, 2, &mut buffer)?;
        
        // Buscar atributo de índice
        let _index_attr = self.read_attribute(&buffer, NtfsAttributeType::IndexRoot)?;
        
        // TODO: Implementar búsqueda en índice
        // Por ahora, simular búsqueda exitosa
        Ok(5) // Retornar número de registro simulado
    }
    
    /// Crear archivo
    pub fn create_file(&mut self, _directory_record: u64, _filename: &str) -> Result<u64, &'static str> {
        // TODO: Implementar creación de archivo
        // Por ahora, simular creación exitosa
        Ok(6) // Retornar número de registro simulado
    }
    
    /// Eliminar archivo
    pub fn delete_file(&mut self, _file_record: u64) -> Result<(), &'static str> {
        // TODO: Implementar eliminación de archivo
        Ok(())
    }
    
    /// Obtener estadísticas del driver
    pub fn get_stats(&self) -> (u32, u32, u64, u64) {
        (self.cache_hits, self.cache_misses, self.total_clusters, self.mft_record_size as u64)
    }
}

/// Instancia global del driver NTFS
static mut NTFS_DRIVER: Option<NtfsDriver> = None;

/// Inicializar driver NTFS
pub fn init_ntfs(device_id: u32) -> Result<(), &'static str> {
    unsafe {
        NTFS_DRIVER = Some(NtfsDriver::new(device_id));
        if let Some(ref mut driver) = NTFS_DRIVER {
            driver.init()?;
        }
    }
    Ok(())
}

/// Obtener driver NTFS
pub fn get_ntfs_driver() -> Option<&'static mut NtfsDriver> {
    unsafe {
        NTFS_DRIVER.as_mut()
    }
}

// ============================================================================
// FUNCIONES ADICIONALES DE NTFS
// ============================================================================

/// Montar volumen NTFS
pub fn mount_ntfs_volume(_device_id: u32, _mount_point: &str) -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar montaje de volumen real
        // Por ahora, simular montaje exitoso
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Desmontar volumen NTFS
pub fn unmount_ntfs_volume(_mount_point: &str) -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar desmontaje de volumen real
        // Por ahora, simular desmontaje exitoso
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Verificar integridad del sistema de archivos NTFS
pub fn check_ntfs_integrity() -> Result<bool, &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar verificación de integridad real
        // Por ahora, simular verificación exitosa
        Ok(true)
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Reparar sistema de archivos NTFS
pub fn repair_ntfs() -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar reparación de NTFS real
        // Por ahora, simular reparación exitosa
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Obtener información del volumen NTFS
pub fn get_ntfs_volume_info() -> Result<(u64, u64, u64), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar obtención de información de volumen real
        // Por ahora, simular información
        Ok((1024 * 1024 * 1024, 512 * 1024 * 1024, 512 * 1024 * 1024)) // Total, Libre, Usado
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Crear directorio en NTFS
pub fn create_ntfs_directory(parent_record: u64, dirname: &str) -> Result<u64, &'static str> {
    if let Some(ref mut driver) = get_ntfs_driver() {
        driver.create_file(parent_record, dirname)
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Eliminar directorio en NTFS
pub fn delete_ntfs_directory(dir_record: u64) -> Result<(), &'static str> {
    if let Some(ref mut driver) = get_ntfs_driver() {
        driver.delete_file(dir_record)
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Listar contenido de directorio NTFS
pub fn list_ntfs_directory(dir_record: u64) -> Result<&'static [&'static str], &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar listado de directorio real
        // Por ahora, simular listado
        Ok(&["file1.txt", "file2.txt", "subdir"])
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Obtener atributos de archivo NTFS
pub fn get_ntfs_file_attributes(_file_record: u64) -> Result<u32, &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar obtención de atributos real
        // Por ahora, simular atributos
        Ok(0x20) // FILE_ATTRIBUTE_ARCHIVE
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Establecer atributos de archivo NTFS
pub fn set_ntfs_file_attributes(_file_record: u64, _attributes: u32) -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar establecimiento de atributos real
        // Por ahora, simular éxito
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Obtener información de tiempo de archivo NTFS
pub fn get_ntfs_file_times(_file_record: u64) -> Result<(i64, i64, i64), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar obtención de tiempos real
        // Por ahora, simular tiempos
        let now = 0; // Timestamp actual simulado
        Ok((now, now, now)) // Creación, Acceso, Modificación
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Establecer información de tiempo de archivo NTFS
pub fn set_ntfs_file_times(_file_record: u64, _creation: i64, _access: i64, _modification: i64) -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar establecimiento de tiempos real
        // Por ahora, simular éxito
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Obtener tamaño de archivo NTFS
pub fn get_ntfs_file_size(_file_record: u64) -> Result<u64, &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar obtención de tamaño real
        // Por ahora, simular tamaño
        Ok(1024) // Tamaño simulado
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Truncar archivo NTFS
pub fn truncate_ntfs_file(_file_record: u64, _new_size: u64) -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar truncado de archivo real
        // Por ahora, simular éxito
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Obtener estadísticas de rendimiento NTFS
pub fn get_ntfs_performance_stats() -> Result<(u32, u32, u64, u64), &'static str> {
    if let Some(ref mut driver) = get_ntfs_driver() {
        Ok(driver.get_stats())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Limpiar caché NTFS
pub fn clear_ntfs_cache() -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar limpieza de caché real
        // Por ahora, simular éxito
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}

/// Sincronizar cambios NTFS
pub fn sync_ntfs_changes() -> Result<(), &'static str> {
    if let Some(ref mut _driver) = get_ntfs_driver() {
        // TODO: Implementar sincronización real
        // Por ahora, simular éxito
        Ok(())
    } else {
        Err("Driver NTFS no inicializado")
    }
}
