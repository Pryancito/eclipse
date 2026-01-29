//! Escritor de imágenes EclipseFS

use crate::{
    format::constants, format::tlv_tags, EclipseFSError, EclipseFSHeader, EclipseFSNode,
    EclipseFSResult, NodeKind,
};
use byteorder::{LittleEndian, WriteBytesExt};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

/// Escritor de imágenes EclipseFS
pub struct EclipseFSWriter {
    file: File,
    nodes: BTreeMap<u32, EclipseFSNode>,
    next_inode: u32,
}

impl EclipseFSWriter {
    /// Crear un nuevo escritor
    pub fn new(file: File) -> Self {
        Self {
            file,
            nodes: BTreeMap::new(),
            next_inode: constants::ROOT_INODE + 1,
        }
    }

    /// Crear un nuevo escritor desde un path
    pub fn from_path(file_path: &str) -> EclipseFSResult<Self> {
        let file = File::create(file_path)?;
        Ok(Self::new(file))
    }

    /// Agregar un nodo
    pub fn add_node(&mut self, inode: u32, node: EclipseFSNode) -> EclipseFSResult<()> {
        if self.nodes.contains_key(&inode) {
            return Err(EclipseFSError::DuplicateEntry);
        }

        self.nodes.insert(inode, node);
        Ok(())
    }

    /// Asignar un nuevo inode
    pub fn allocate_inode(&mut self) -> u32 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }

    /// Escribir la imagen completa
    pub fn write_image(&mut self) -> EclipseFSResult<()> {
        let total_inodes = self.nodes.len() as u32;

        // Escribir header
        let header = EclipseFSHeader::new(total_inodes);
        self.write_header(&header)?;

        // Escribir tabla de inodos
        self.write_inode_table(&header)?;

        // Escribir nodos
        self.write_nodes(&header)?;

        Ok(())
    }

    /// Escribir el header
    fn write_header(&mut self, header: &EclipseFSHeader) -> EclipseFSResult<()> {
        self.file.write_all(&header.magic)?;
        self.file.write_u32::<LittleEndian>(header.version)?;
        self.file
            .write_u64::<LittleEndian>(header.inode_table_offset)?;
        self.file
            .write_u64::<LittleEndian>(header.inode_table_size)?;
        self.file.write_u32::<LittleEndian>(header.total_inodes)?;

        // Rellenar hasta el tamaño del header
        let header_size = EclipseFSHeader::size();
        let written = 9 + 4 + 8 + 8 + 4; // magic + version + offset + size + total_inodes
        let padding = header_size - written;
        if padding > 0 {
            self.file.write_all(&vec![0u8; padding])?;
        }

        Ok(())
    }

    /// Escribir la tabla de inodos
    fn write_inode_table(&mut self, header: &EclipseFSHeader) -> EclipseFSResult<()> {
        self.file.seek(SeekFrom::Start(header.inode_table_offset))?;

        let nodes_start = header.inode_table_offset + header.inode_table_size;

        // Precalcular el orden y el tamaño acumulado de los nodos
        let mut nodes_sorted: Vec<(u32, usize)> = self
            .nodes
            .iter()
            .map(|(&inode, node)| (inode, self.calculate_node_size(node)))
            .collect();
        nodes_sorted.sort_by_key(|(inode, _)| *inode);

        let mut current_offset = nodes_start;
        for (inode, size) in nodes_sorted.iter() {
            self.file.write_u32::<LittleEndian>(*inode)?;
            let relative_offset = (current_offset - nodes_start) as u32;
            self.file.write_u32::<LittleEndian>(relative_offset)?;
            current_offset += *size as u64;
        }

        Ok(())
    }

    /// Escribir todos los nodos
    fn write_nodes(&mut self, header: &EclipseFSHeader) -> EclipseFSResult<()> {
        let mut current_offset = header.inode_table_offset + header.inode_table_size;

        let mut nodes_sorted: Vec<(u32, EclipseFSNode)> =
            self.nodes.iter().map(|(&inode, node)| (inode, node.clone())).collect();
        nodes_sorted.sort_by_key(|(inode, _)| *inode);

        for (inode, node) in nodes_sorted.iter() {
            self.file.seek(SeekFrom::Start(current_offset))?;
            self.write_node(*inode, node)?;

            let node_size = self.calculate_node_size(node);
            current_offset += node_size as u64;
        }

        Ok(())
    }

    /// Escribir un nodo individual
    fn write_node(&mut self, inode: u32, node: &EclipseFSNode) -> EclipseFSResult<()> {
        // Encabezado: inode (u32) + tamaño del registro (u32, incluyendo este encabezado)
        let node_size = self.calculate_node_size(node) as u32;
        self.file.write_u32::<LittleEndian>(inode)?;
        self.file.write_u32::<LittleEndian>(node_size)?;

        // NODE_TYPE
        let node_type = match node.kind {
            NodeKind::File => 1u8,
            NodeKind::Directory => 2u8,
            NodeKind::Symlink => 3u8,
        };
        self.write_tlv_entry(tlv_tags::NODE_TYPE, &[node_type])?;

        // MODE
        self.write_tlv_entry(tlv_tags::MODE, &node.mode.to_le_bytes())?;

        // UID
        self.write_tlv_entry(tlv_tags::UID, &node.uid.to_le_bytes())?;

        // GID
        self.write_tlv_entry(tlv_tags::GID, &node.gid.to_le_bytes())?;

        // SIZE
        self.write_tlv_entry(tlv_tags::SIZE, &node.size.to_le_bytes())?;

        // ATIME
        self.write_tlv_entry(tlv_tags::ATIME, &node.atime.to_le_bytes())?;

        // MTIME
        self.write_tlv_entry(tlv_tags::MTIME, &node.mtime.to_le_bytes())?;

        // CTIME
        self.write_tlv_entry(tlv_tags::CTIME, &node.ctime.to_le_bytes())?;

        // NLINK
        self.write_tlv_entry(tlv_tags::NLINK, &node.nlink.to_le_bytes())?;

        // CONTENT
        if !node.data.is_empty() {
            self.write_tlv_entry(tlv_tags::CONTENT, &node.data)?;
        }

        // DIRECTORY_ENTRIES
        if !node.children.is_empty() {
            let entries_data = self.serialize_directory_entries(&node.children)?;
            self.write_tlv_entry(tlv_tags::DIRECTORY_ENTRIES, &entries_data)?;
        }

        Ok(())
    }

    /// Escribir una entrada TLV
    fn write_tlv_entry(&mut self, tag: u16, value: &[u8]) -> EclipseFSResult<()> {
        self.file.write_u16::<LittleEndian>(tag)?;
        self.file.write_u32::<LittleEndian>(value.len() as u32)?;
        self.file.write_all(value)?;
        Ok(())
    }

    /// Serializar entradas de directorio
    fn serialize_directory_entries(
        &self,
        children: &std::collections::HashMap<String, u32>,
    ) -> EclipseFSResult<Vec<u8>> {
        let mut data = Vec::new();

        for (name, inode) in children {
            let name_bytes = name.as_bytes();
            data.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            data.extend_from_slice(&inode.to_le_bytes());
            data.extend_from_slice(name_bytes);
        }

        Ok(data)
    }

    /// Calcular el tamaño de un nodo
    fn calculate_node_size(&self, node: &EclipseFSNode) -> usize {
        let mut size = constants::NODE_RECORD_HEADER_SIZE;

        // NODE_TYPE (2 + 4 + 1)
        size += 7;

        // MODE (2 + 4 + 4)
        size += 10;

        // UID (2 + 4 + 4)
        size += 10;

        // GID (2 + 4 + 4)
        size += 10;

        // SIZE (2 + 4 + 8)
        size += 14;

        // ATIME (2 + 4 + 8)
        size += 14;

        // MTIME (2 + 4 + 8)
        size += 14;

        // CTIME (2 + 4 + 8)
        size += 14;

        // NLINK (2 + 4 + 4)
        size += 10;

        // CONTENT (2 + 4 + data_len)
        if !node.data.is_empty() {
            size += 6 + node.data.len();
        }

        // DIRECTORY_ENTRIES (2 + 4 + entries_data_len)
        if !node.children.is_empty() {
            let entries_data = self
                .serialize_directory_entries(&node.children)
                .unwrap_or_default();
            size += 6 + entries_data.len();
        }

        size
    }

    /// Crear el nodo raíz
    pub fn create_root(&mut self) -> EclipseFSResult<()> {
        let root = EclipseFSNode::new_dir();
        self.add_node(constants::ROOT_INODE, root)?;
        Ok(())
    }

    /// Crear un nuevo nodo y retornar su inode
    pub fn create_node(&mut self, node: EclipseFSNode) -> EclipseFSResult<u32> {
        let inode = self.allocate_inode();
        self.add_node(inode, node)?;
        Ok(inode)
    }

    /// Obtener el nodo raíz
    pub fn get_root(&mut self) -> EclipseFSResult<&mut EclipseFSNode> {
        self.nodes
            .get_mut(&constants::ROOT_INODE)
            .ok_or(EclipseFSError::NotFound)
    }

    /// Obtener un nodo por su inode
    pub fn get_node(&mut self, inode: u32) -> EclipseFSResult<&mut EclipseFSNode> {
        self.nodes.get_mut(&inode).ok_or(EclipseFSError::NotFound)
    }
}
