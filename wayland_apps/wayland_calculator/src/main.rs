//! Aplicación de calculadora Wayland para Eclipse OS
//! 
//! Esta aplicación implementa una calculadora básica que se conecta al compositor Wayland
//! y proporciona una interfaz de calculadora simple.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;
use core::fmt::Write;
use core::str::FromStr;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use linked_list_allocator::LockedHeap;

/// Tamaño del heap (1MB)
const HEAP_SIZE: usize = 1024 * 1024;

/// Heap global
#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

/// Inicializa el allocator
fn init_allocator() {
    unsafe {
        static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
        HEAP.lock().init(HEAP_MEM.as_mut_ptr(), HEAP_SIZE);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

/// Operaciones matemáticas disponibles
#[derive(Debug, Clone, Copy)]
pub enum Operation {
    Add,
    Subtract,
    Multiply,
    Divide,
    Power,
    SquareRoot,
}

/// Estados de la calculadora
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CalculatorState {
    WaitingForFirstOperand,
    WaitingForOperation,
    WaitingForSecondOperand,
    DisplayingResult,
    Error,
}

/// Estructura para manejar la aplicación de calculadora Wayland
pub struct WaylandCalculator {
    /// ID de la superficie Wayland
    surface_id: u32,
    /// Display actual de la calculadora
    display: String,
    /// Primer operando
    first_operand: f64,
    /// Operación actual
    current_operation: Option<Operation>,
    /// Estado actual de la calculadora
    state: CalculatorState,
    /// Historial de operaciones
    history: Vec<String>,
    /// Ancho de la ventana
    width: u32,
    /// Alto de la ventana
    height: u32,
}

impl WaylandCalculator {
    /// Crea una nueva instancia de la calculadora Wayland
    pub fn new() -> Self {
        Self {
            surface_id: 0,
            display: String::new(),
            first_operand: 0.0,
            current_operation: None,
            state: CalculatorState::WaitingForFirstOperand,
            history: Vec::new(),
            width: 300,
            height: 400,
        }
    }

    /// Inicializa la calculadora y se conecta al compositor Wayland
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Simular conexión al compositor Wayland
        self.surface_id = 1;
        
        // Mostrar pantalla inicial
        self.display = "0".to_string();
        
        Ok(())
    }

    /// Procesa un dígito ingresado
    pub fn input_digit(&mut self, digit: u8) {
        if digit > 9 {
            return;
        }

        match self.state {
            CalculatorState::WaitingForFirstOperand | CalculatorState::DisplayingResult => {
                if self.display == "0" || self.state == CalculatorState::DisplayingResult {
                    self.display = digit.to_string();
                } else {
                    self.display.push_str(&digit.to_string());
                }
                self.state = CalculatorState::WaitingForOperation;
            },
            CalculatorState::WaitingForSecondOperand => {
                if self.display == "0" {
                    self.display = digit.to_string();
                } else {
                    self.display.push_str(&digit.to_string());
                }
            },
            _ => {}
        }
    }

    /// Procesa una operación matemática
    pub fn input_operation(&mut self, operation: Operation) {
        if let Ok(value) = self.display.parse::<f64>() {
            self.first_operand = value;
            self.current_operation = Some(operation);
            self.state = CalculatorState::WaitingForSecondOperand;
            self.display = "0".to_string();
        }
    }

    /// Calcula el resultado
    pub fn calculate(&mut self) {
        if let Some(operation) = self.current_operation {
            if let Ok(second_operand) = self.display.parse::<f64>() {
                let result = match operation {
                    Operation::Add => self.first_operand + second_operand,
                    Operation::Subtract => self.first_operand - second_operand,
                    Operation::Multiply => self.first_operand * second_operand,
                    Operation::Divide => {
                        if second_operand != 0.0 {
                            self.first_operand / second_operand
                        } else {
                            self.state = CalculatorState::Error;
                            self.display = "Error: División por cero".to_string();
                            return;
                        }
                    },
                    Operation::Power => libm::pow(self.first_operand, second_operand),
                    Operation::SquareRoot => {
                        if self.first_operand >= 0.0 {
                            libm::sqrt(self.first_operand)
                        } else {
                            self.state = CalculatorState::Error;
                            self.display = "Error: Raíz de número negativo".to_string();
                            return;
                        }
                    },
                };

                // Agregar al historial
                self.add_to_history(&format!("{} {} {} = {}", 
                    self.first_operand, 
                    self.operation_to_string(operation), 
                    second_operand, 
                    result
                ));

                self.display = result.to_string();
                self.state = CalculatorState::DisplayingResult;
                self.current_operation = None;
            }
        }
    }

    /// Limpia la calculadora
    pub fn clear(&mut self) {
        self.display = "0".to_string();
        self.first_operand = 0.0;
        self.current_operation = None;
        self.state = CalculatorState::WaitingForFirstOperand;
    }

    /// Borra el último dígito
    pub fn backspace(&mut self) {
        if self.display.len() > 1 {
            self.display.pop();
        } else {
            self.display = "0".to_string();
        }
    }

    /// Agrega una entrada al historial
    fn add_to_history(&mut self, entry: &str) {
        self.history.push(entry.to_string());
    }

    /// Convierte una operación a string
    fn operation_to_string(&self, operation: Operation) -> &'static str {
        match operation {
            Operation::Add => "+",
            Operation::Subtract => "-",
            Operation::Multiply => "×",
            Operation::Divide => "÷",
            Operation::Power => "^",
            Operation::SquareRoot => "√",
        }
    }

    /// Renderiza la calculadora en la superficie Wayland
    pub fn render(&mut self) -> Result<(), &'static str> {
        // Simular renderizado en la superficie Wayland
        // En una implementación real, aquí se actualizaría el buffer de la superficie
        
        Ok(())
    }

    /// Obtiene el display actual
    pub fn get_display(&self) -> &str {
        &self.display
    }

    /// Obtiene el historial
    pub fn get_history(&self) -> &[String] {
        &self.history
    }
}

/// Función principal de la aplicación
#[no_mangle]
pub extern "C" fn main() -> ! {
    // Inicializar el allocator
    init_allocator();
    
    let mut calculator = WaylandCalculator::new();
    
    // Inicializar la calculadora
    if let Err(_e) = calculator.initialize() {
        loop {}
    }
    
    // Simular entrada de usuario
    calculator.input_digit(1);
    calculator.input_digit(2);
    calculator.input_operation(Operation::Add);
    calculator.input_digit(3);
    calculator.input_digit(4);
    calculator.calculate();
    
    // Renderizar la calculadora
    if let Err(_e) = calculator.render() {
        loop {}
    }
    
    // Bucle infinito
    loop {}
}