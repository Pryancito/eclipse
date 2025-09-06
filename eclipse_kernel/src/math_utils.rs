//! Utilidades matemáticas para Eclipse OS
//!
//! Este módulo proporciona funciones matemáticas básicas
//! que no están disponibles en no_std

/// Función sqrt para f64
pub fn sqrt(x: f64) -> f64 {
    if x < 0.0 {
        return 0.0;
    }
    
    if x == 0.0 {
        return 0.0;
    }
    
    // Implementación simple de Newton-Raphson para sqrt
    let mut guess = x / 2.0;
    let mut prev_guess = 0.0;
    
    while (guess - prev_guess).abs() > 1e-10 {
        prev_guess = guess;
        guess = (guess + x / guess) / 2.0;
    }
    
    guess
}

/// Función max para f64
pub fn max_f64(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// Función min para f64
pub fn min_f64(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

