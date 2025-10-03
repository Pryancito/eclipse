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
    if a > b {
        a
    } else {
        b
    }
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

    x - x3 / 6.0 + x5 / 120.0 - x7 / 5040.0 + x9 / 362880.0
}

/// Función min para f32
pub fn min(a: f32, b: f32) -> f32 {
    if a < b {
        a
    } else {
        b
    }
}

/// Función max para f32
pub fn max(a: f32, b: f32) -> f32 {
    if a > b {
        a
    } else {
        b
    }
}

/// Función min para f64
pub fn min_f64(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else {
        b
    }
}

/// Función atan2 para f32
pub fn atan2(y: f32, x: f32) -> f32 {
    if x == 0.0 {
        if y > 0.0 {
            core::f32::consts::PI / 2.0
        } else if y < 0.0 {
            -core::f32::consts::PI / 2.0
        } else {
            0.0
        }
    } else {
        // Aproximación simple de atan usando serie de Taylor
        let ratio = y / x;
        let atan = if ratio.abs() < 1.0 {
            let ratio2 = ratio * ratio;
            let ratio3 = ratio2 * ratio;
            let ratio5 = ratio3 * ratio2;
            let ratio7 = ratio5 * ratio2;
            ratio - ratio3 / 3.0 + ratio5 / 5.0 - ratio7 / 7.0
        } else {
            let inv_ratio = 1.0 / ratio;
            let inv_ratio3 = inv_ratio * inv_ratio * inv_ratio;
            let inv_ratio5 = inv_ratio3 * inv_ratio * inv_ratio;
            core::f32::consts::PI / 2.0 - inv_ratio + inv_ratio3 / 3.0 - inv_ratio5 / 5.0
        };

        if x < 0.0 {
            if y >= 0.0 {
                atan + core::f32::consts::PI
            } else {
                atan - core::f32::consts::PI
            }
        } else {
            atan
        }
    }
}
