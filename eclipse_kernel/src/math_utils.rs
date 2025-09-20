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

/// Función sin para f32
pub fn sin(x: f32) -> f32 {
    // Aproximación simple usando serie de Taylor
    let x = x % (2.0 * core::f32::consts::PI);
    let x2 = x * x;
    let x3 = x2 * x;
    let x5 = x3 * x2;
    let x7 = x5 * x2;
    let x9 = x7 * x2;
    
    x - x3/6.0 + x5/120.0 - x7/5040.0 + x9/362880.0
}

/// Función min para f32
pub fn min(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

/// Función max para f32
pub fn max(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// Función min para f64
pub fn min_f64(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

