#![allow(dead_code)]
//! Calculadora científica para Eclipse OS
//!
//! Proporciona funciones matemáticas básicas y avanzadas
//! con soporte para expresiones complejas.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Operador matemático
#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Power,
    Modulo,
    Factorial,
    SquareRoot,
    Sine,
    Cosine,
    Tangent,
    Logarithm,
    NaturalLog,
}

/// Token de expresión matemática
#[derive(Debug, Clone)]
pub enum Token {
    Number(f64),
    Operator(Operator),
    LeftParen,
    RightParen,
    Variable(String),
}

/// Calculadora científica
pub struct Calculator {
    variables: Vec<(String, f64)>,
    history: Vec<String>,
    precision: usize,
}

impl Calculator {
    pub fn new() -> Self {
        Self {
            variables: Vec::new(),
            history: Vec::new(),
            precision: 6,
        }
    }

    /// Ejecutar la calculadora
    pub fn run(&mut self) -> Result<(), &'static str> {
        self.show_welcome();

        loop {
            self.show_prompt();
            let input = self.read_input();

            if input.trim().is_empty() {
                continue;
            }

            if input.trim() == "exit" {
                break;
            }

            match self.evaluate_expression(&input) {
                Ok(result) => {
                    let result_str = format!("{:.6}", result);
                    self.print_result(&result_str);
                    self.history.push(format!("{} = {}", input, result_str));
                }
                Err(e) => {
                    self.print_error(&format!("Error: {}", e));
                }
            }
        }

        Ok(())
    }

    fn show_welcome(&self) {
        self.print_info("╔══════════════════════════════════════════════════════════════╗");
        self.print_info("║                                                              ║");
        self.print_info("║                    ECLIPSE CALCULATOR                        ║");
        self.print_info("║                                                              ║");
        self.print_info("║  Calculadora científica con funciones avanzadas             ║");
        self.print_info("║  Escribe 'help' para ver comandos disponibles              ║");
        self.print_info("║  Escribe 'exit' para salir                                 ║");
        self.print_info("║                                                              ║");
        self.print_info("╚══════════════════════════════════════════════════════════════╝");
        self.print_info("");
    }

    fn show_prompt(&self) {
        self.print_info("calc> ");
    }

    fn read_input(&self) -> String {
        // En una implementación real, esto leería del teclado
        // Por ahora simulamos con un input fijo
        "2 + 2".to_string()
    }

    fn evaluate_expression(&mut self, expr: &str) -> Result<f64, &'static str> {
        if expr.trim() == "help" {
            self.show_help();
            return Ok(0.0);
        }

        if expr.trim() == "history" {
            self.show_history();
            return Ok(0.0);
        }

        if expr.trim() == "clear" {
            self.clear_history();
            return Ok(0.0);
        }

        if expr.trim() == "vars" {
            self.show_variables();
            return Ok(0.0);
        }

        // Parsear la expresión
        let tokens = self.parse_expression(expr)?;

        // Evaluar la expresión
        self.evaluate_tokens(&tokens)
    }

    fn parse_expression(&self, expr: &str) -> Result<Vec<Token>, &'static str> {
        let mut tokens = Vec::new();
        let mut chars = expr.chars().peekable();
        let mut current_number = String::new();

        while let Some(ch) = chars.next() {
            match ch {
                '0'..='9' | '.' => {
                    current_number.push(ch);
                }
                '+' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::Operator(Operator::Add));
                }
                '-' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::Operator(Operator::Subtract));
                }
                '*' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::Operator(Operator::Multiply));
                }
                '/' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::Operator(Operator::Divide));
                }
                '^' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::Operator(Operator::Power));
                }
                '%' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::Operator(Operator::Modulo));
                }
                '!' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::Operator(Operator::Factorial));
                }
                '(' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::LeftParen);
                }
                ')' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                    tokens.push(Token::RightParen);
                }
                ' ' => {
                    if !current_number.is_empty() {
                        tokens.push(Token::Number(
                            current_number.parse().map_err(|_| "Número inválido")?,
                        ));
                        current_number.clear();
                    }
                }
                _ => {
                    // Verificar si es una función
                    if ch.is_alphabetic() {
                        let mut func_name = String::new();
                        func_name.push(ch);

                        while let Some(&next_ch) = chars.peek() {
                            if next_ch.is_alphabetic() {
                                func_name.push(chars.next().unwrap());
                            } else {
                                break;
                            }
                        }

                        match func_name.as_str() {
                            "sin" => tokens.push(Token::Operator(Operator::Sine)),
                            "cos" => tokens.push(Token::Operator(Operator::Cosine)),
                            "tan" => tokens.push(Token::Operator(Operator::Tangent)),
                            "log" => tokens.push(Token::Operator(Operator::Logarithm)),
                            "ln" => tokens.push(Token::Operator(Operator::NaturalLog)),
                            "sqrt" => tokens.push(Token::Operator(Operator::SquareRoot)),
                            _ => return Err("Función no reconocida"),
                        }
                    }
                }
            }
        }

        if !current_number.is_empty() {
            tokens.push(Token::Number(
                current_number.parse().map_err(|_| "Número inválido")?,
            ));
        }

        Ok(tokens)
    }

    fn evaluate_tokens(&mut self, tokens: &[Token]) -> Result<f64, &'static str> {
        // Implementación simplificada de evaluación de expresiones
        // En una implementación real, esto usaría el algoritmo shunting yard

        if tokens.is_empty() {
            return Err("Expresión vacía");
        }

        let mut stack = Vec::new();
        let mut i = 0;

        while i < tokens.len() {
            match &tokens[i] {
                Token::Number(n) => {
                    stack.push(*n);
                }
                Token::Operator(op) => {
                    if stack.len() < 2 {
                        return Err("Operador sin suficientes operandos");
                    }

                    let b = stack.pop().unwrap();
                    let a = stack.pop().unwrap();

                    let result = match op {
                        Operator::Add => a + b,
                        Operator::Subtract => a - b,
                        Operator::Multiply => a * b,
                        Operator::Divide => {
                            if b == 0.0 {
                                return Err("División por cero");
                            }
                            a / b
                        }
                        Operator::Power => self.power(a, b),
                        Operator::Modulo => a % b,
                        Operator::Factorial => self.factorial(a as u64) as f64,
                        Operator::SquareRoot => self.sqrt(a),
                        Operator::Sine => self.sin(a.to_radians()),
                        Operator::Cosine => self.cos(a.to_radians()),
                        Operator::Tangent => self.tan(a.to_radians()),
                        Operator::Logarithm => self.log10(a),
                        Operator::NaturalLog => self.ln(a),
                    };

                    stack.push(result);
                }
                _ => return Err("Token no soportado en esta posición"),
            }
            i += 1;
        }

        if stack.len() != 1 {
            return Err("Expresión mal formada");
        }

        Ok(stack[0])
    }

    fn factorial(&self, n: u64) -> u64 {
        if n <= 1 {
            1
        } else {
            n * self.factorial(n - 1)
        }
    }

    fn power(&self, base: f64, exp: f64) -> f64 {
        // Implementación simple de potencia
        if exp == 0.0 {
            return 1.0;
        }
        if exp == 1.0 {
            return base;
        }
        if exp == 2.0 {
            return base * base;
        }
        // Para exponentes más complejos, usar aproximación
        self.exp(exp * self.ln(base))
    }

    fn sqrt(&self, x: f64) -> f64 {
        if x < 0.0 {
            return f64::NAN;
        }
        if x == 0.0 {
            return 0.0;
        }
        // Método de Newton para raíz cuadrada
        let mut guess = x / 2.0;
        for _ in 0..10 {
            guess = (guess + x / guess) / 2.0;
        }
        guess
    }

    fn sin(&self, x: f64) -> f64 {
        // Aproximación de Taylor para seno
        let mut result = x;
        let mut term = x;
        for i in 1..10 {
            term *= -x * x / ((2 * i) * (2 * i + 1)) as f64;
            result += term;
        }
        result
    }

    fn cos(&self, x: f64) -> f64 {
        // Aproximación de Taylor para coseno
        let mut result = 1.0;
        let mut term = 1.0;
        for i in 1..10 {
            term *= -x * x / ((2 * i - 1) * (2 * i)) as f64;
            result += term;
        }
        result
    }

    fn tan(&self, x: f64) -> f64 {
        let sin_x = self.sin(x);
        let cos_x = self.cos(x);
        if cos_x == 0.0 {
            f64::INFINITY
        } else {
            sin_x / cos_x
        }
    }

    fn log10(&self, x: f64) -> f64 {
        if x <= 0.0 {
            f64::NAN
        } else {
            self.ln(x) / self.ln(10.0)
        }
    }

    fn ln(&self, x: f64) -> f64 {
        if x <= 0.0 {
            f64::NAN
        } else {
            // Aproximación de ln usando serie de Taylor
            if x == 1.0 {
                return 0.0;
            }
            let y = (x - 1.0) / (x + 1.0);
            let mut result = 2.0 * y;
            let mut term = y * y;
            for i in 1..20 {
                result += 2.0 * term / (2 * i + 1) as f64;
                term *= y * y;
            }
            result
        }
    }

    fn exp(&self, x: f64) -> f64 {
        // Aproximación de e^x usando serie de Taylor
        let mut result = 1.0;
        let mut term = 1.0;
        for i in 1..20 {
            term *= x / i as f64;
            result += term;
        }
        result
    }

    fn show_help(&self) {
        self.print_info("Comandos disponibles:");
        self.print_info("  help          - Muestra esta ayuda");
        self.print_info("  exit          - Sale de la calculadora");
        self.print_info("  history       - Muestra el historial");
        self.print_info("  clear         - Limpia el historial");
        self.print_info("  vars          - Muestra variables");
        self.print_info("");
        self.print_info("Operadores:");
        self.print_info("  +             - Suma");
        self.print_info("  -             - Resta");
        self.print_info("  *             - Multiplicación");
        self.print_info("  /             - División");
        self.print_info("  ^             - Potencia");
        self.print_info("  %             - Módulo");
        self.print_info("  !             - Factorial");
        self.print_info("");
        self.print_info("Funciones:");
        self.print_info("  sin(x)        - Seno");
        self.print_info("  cos(x)        - Coseno");
        self.print_info("  tan(x)        - Tangente");
        self.print_info("  log(x)        - Logaritmo base 10");
        self.print_info("  ln(x)         - Logaritmo natural");
        self.print_info("  sqrt(x)       - Raíz cuadrada");
        self.print_info("");
        self.print_info("Ejemplos:");
        self.print_info("  2 + 3 * 4");
        self.print_info("  sin(45)");
        self.print_info("  sqrt(16)");
        self.print_info("  5! + 3^2");
    }

    fn show_history(&self) {
        self.print_info("Historial de cálculos:");
        for (i, entry) in self.history.iter().enumerate() {
            self.print_info(&format!("  {}: {}", i + 1, entry));
        }
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.print_info("Historial limpiado");
    }

    fn show_variables(&self) {
        if self.variables.is_empty() {
            self.print_info("No hay variables definidas");
        } else {
            self.print_info("Variables definidas:");
            for (name, value) in &self.variables {
                self.print_info(&format!("  {} = {}", name, value));
            }
        }
    }

    fn print_info(&self, text: &str) {
        // En una implementación real, esto imprimiría en la consola
        // Por ahora solo simulamos
    }

    fn print_result(&self, text: &str) {
        // En una implementación real, esto imprimiría en la consola con color verde
        // Por ahora solo simulamos
    }

    fn print_error(&self, text: &str) {
        // En una implementación real, esto imprimiría en la consola con color rojo
        // Por ahora solo simulamos
    }
}

/// Función principal para ejecutar la calculadora
pub fn run() -> Result<(), &'static str> {
    let mut calculator = Calculator::new();
    calculator.run()
}
