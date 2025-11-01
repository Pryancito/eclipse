//! Un búfer circular SPSC (Single-Producer, Single-Consumer) sin bloqueos.
//!
//! Esta estructura de datos permite que un hilo (el productor) escriba datos
//! y otro hilo (el consumidor) los lea de forma segura y concurrente sin
//! necesidad de usar locks (Mutex, Spinlock), lo cual es crucial para la
//! comunicación entre rutinas de interrupción (ISRs) y el código normal del kernel.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Error que puede ocurrir al usar el RingBuffer.
#[derive(Debug, PartialEq, Eq)]
pub enum RingBufferError {
    /// El búfer está lleno, no se puede escribir un nuevo elemento.
    Full,
    /// El búfer está vacío, no se puede leer un elemento.
    Empty,
}

/// Un búfer circular SPSC (Single-Producer, Single-Consumer) sin bloqueos.
///
/// `T` es el tipo de dato almacenado en el búfer.
/// `N` es la capacidad del búfer, que debe ser una potencia de dos.
pub struct RingBuffer<T, const N: usize> {
    /// El almacenamiento subyacente para los elementos del búfer.
    /// `UnsafeCell` permite la mutación interior sin un `&mut self`.
    buffer: [UnsafeCell<T>; N],
    /// Índice donde el productor escribirá el próximo elemento.
    head: AtomicUsize,
    /// Índice desde donde el consumidor leerá el próximo elemento.
    tail: AtomicUsize,
}

// Aseguramos que RingBuffer sea seguro para enviar entre hilos si T lo es.
unsafe impl<T: Send, const N: usize> Send for RingBuffer<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for RingBuffer<T, N> {}

impl<T: Copy + Default, const N: usize> RingBuffer<T, N> {
    /// Crea un nuevo `RingBuffer` vacío.
    ///
    /// # Panics
    /// `panic!` si `N` no es una potencia de dos.
    pub const fn new() -> Self {
        assert!(N.is_power_of_two(), "La capacidad del RingBuffer debe ser una potencia de dos");
        Self {
            buffer: [const { UnsafeCell::new(T::default()) }; N],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Escribe un elemento en el búfer.
    ///
    /// Esta operación es para el **productor**. Es segura para ser llamada desde
    /// una interrupción. Usa `Ordering::Relaxed` porque solo el productor
    /// modifica `head`, y no se requiere sincronización inmediata con el consumidor.
    ///
    /// # Returns
    /// `Ok(())` si el elemento se escribió correctamente.
    /// `Err(RingBufferError::Full)` si el búfer está lleno.
    pub fn push(&self, item: T) -> Result<(), RingBufferError> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        if (head.wrapping_sub(tail)) == N {
            return Err(RingBufferError::Full);
        }

        let index = head & (N - 1);
        unsafe {
            // Es seguro escribir porque solo el productor accede a esta posición
            // y hemos comprobado que no alcanzará al `tail`.
            *self.buffer[index].get() = item;
        }

        self.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Lee un elemento del búfer.
    ///
    /// Esta operación es para el **consumidor**. Es segura para ser llamada
    /// desde el código principal del kernel.
    ///
    /// # Returns
    /// `Ok(T)` con el elemento leído si el búfer no está vacío.
    /// `Err(RingBufferError::Empty)` si el búfer está vacío.
    pub fn pop(&self) -> Result<T, RingBufferError> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        if head == tail {
            return Err(RingBufferError::Empty);
        }

        let index = tail & (N - 1);
        let item = unsafe {
            // Es seguro leer porque solo el consumidor accede a esta posición
            // y hemos comprobado que no ha alcanzado al `head`.
            *self.buffer[index].get()
        };

        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(item)
    }

    /// Devuelve `true` si el búfer está vacío.
    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }

    /// Devuelve `true` si el búfer está lleno.
    pub fn is_full(&self) -> bool {
        (self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))) == N
    }
}
