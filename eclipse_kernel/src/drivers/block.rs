//! Abstracción para dispositivos de almacenamiento en bloque (Block Devices).

use core::any::Any;

pub trait BlockDevice: Any {
    /// Lee uno o más bloques desde el dispositivo.
    ///
    /// # Argumentos
    /// * `block_address`: La dirección del primer bloque a leer (LBA).
    /// * `buffer`: El buffer donde se guardarán los datos leídos. Debe tener
    ///           un tamaño múltiplo del tamaño de bloque del dispositivo.
    ///
    /// # Retorna
    /// `Ok(())` si la lectura fue exitosa, o un error en caso contrario.
    fn read_blocks(&self, block_address: u64, buffer: &mut [u8]) -> Result<(), &'static str>;

    /// Escribe uno o más bloques en el dispositivo.
    ///
    /// # Argumentos
    /// * `block_address`: La dirección del primer bloque a escribir (LBA).
    /// * `buffer`: El buffer con los datos a escribir. Debe tener
    ///           un tamaño múltiplo del tamaño de bloque del dispositivo.
    ///
    /// # Retorna
    /// `Ok(())` si la escritura fue exitosa, o un error en caso contrario.
    fn write_blocks(&mut self, block_address: u64, buffer: &[u8]) -> Result<(), &'static str>;

    /// Devuelve el tamaño de un bloque en bytes.
    fn block_size(&self) -> u32;

    /// Devuelve el número total de bloques en el dispositivo.
    fn block_count(&self) -> u64;
    
    /// Obtiene una referencia Any para downcasting
    fn as_any(&self) -> &dyn Any;
}
