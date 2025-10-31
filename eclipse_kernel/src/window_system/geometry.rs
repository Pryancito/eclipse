//! Geometría de ventanas y superficies
//!
//! Define estructuras para manejar posiciones, tamaños y transformaciones
//! de ventanas en el sistema de ventanas.

// Importar funciones matemáticas para no_std
#[cfg(not(feature = "std"))]
fn sqrt(x: f32) -> f32 {
    // Implementación simple de sqrt para no_std
    if x < 0.0 {
        return 0.0;
    }
    if x == 0.0 {
        return 0.0;
    }

    let mut guess = x / 2.0;
    for _ in 0..10 {
        guess = (guess + x / guess) / 2.0;
    }
    guess
}

#[cfg(feature = "std")]
fn sqrt(x: f32) -> f32 {
    x.sqrt()
}

/// Punto 2D
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Distancia al origen
    pub fn magnitude(&self) -> f32 {
        sqrt((self.x * self.x + self.y * self.y) as f32)
    }

    /// Distancia a otro punto
    pub fn distance_to(&self, other: &Point) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        sqrt((dx * dx + dy * dy) as f32)
    }
}

/// Tamaño 2D
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Área total
    pub fn area(&self) -> u32 {
        self.width * self.height
    }

    /// Verificar si el tamaño es válido
    pub fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// Redimensionar manteniendo la proporción
    pub fn scale_to_fit(&self, max_size: &Size) -> Size {
        if self.width <= max_size.width && self.height <= max_size.height {
            return *self;
        }

        let width_ratio = max_size.width as f32 / self.width as f32;
        let height_ratio = max_size.height as f32 / self.height as f32;
        let ratio = width_ratio.min(height_ratio);

        Size {
            width: (self.width as f32 * ratio) as u32,
            height: (self.height as f32 * ratio) as u32,
        }
    }
}

/// Rectángulo 2D
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rectangle {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Crear rectángulo desde punto y tamaño
    pub fn from_point_size(point: Point, size: Size) -> Self {
        Self {
            x: point.x,
            y: point.y,
            width: size.width,
            height: size.height,
        }
    }

    /// Esquina superior izquierda
    pub fn top_left(&self) -> Point {
        Point {
            x: self.x,
            y: self.y,
        }
    }

    /// Esquina inferior derecha
    pub fn bottom_right(&self) -> Point {
        Point {
            x: self.x + self.width as i32,
            y: self.y + self.height as i32,
        }
    }

    /// Centro del rectángulo
    pub fn center(&self) -> Point {
        Point {
            x: self.x + (self.width as i32) / 2,
            y: self.y + (self.height as i32) / 2,
        }
    }

    /// Tamaño del rectángulo
    pub fn size(&self) -> Size {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    /// Área del rectángulo
    pub fn area(&self) -> u32 {
        self.width * self.height
    }

    /// Verificar si un punto está dentro del rectángulo
    pub fn contains_point(&self, point: &Point) -> bool {
        point.x >= self.x
            && point.x < self.x + self.width as i32
            && point.y >= self.y
            && point.y < self.y + self.height as i32
    }

    /// Verificar si otro rectángulo está completamente dentro
    pub fn contains_rectangle(&self, other: &Rectangle) -> bool {
        self.contains_point(&other.top_left()) && self.contains_point(&other.bottom_right())
    }

    /// Verificar si dos rectángulos se intersectan
    pub fn intersects(&self, other: &Rectangle) -> bool {
        !(self.x >= other.x + other.width as i32
            || other.x >= self.x + self.width as i32
            || self.y >= other.y + other.height as i32
            || other.y >= self.y + self.height as i32)
    }

    /// Calcular la intersección con otro rectángulo
    pub fn intersection(&self, other: &Rectangle) -> Option<Rectangle> {
        if !self.intersects(other) {
            return None;
        }

        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = (self.x + self.width as i32).min(other.x + other.width as i32);
        let bottom = (self.y + self.height as i32).min(other.y + other.height as i32);

        Some(Rectangle {
            x: left,
            y: top,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        })
    }

    /// Unión con otro rectángulo
    pub fn union(&self, other: &Rectangle) -> Rectangle {
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = (self.x + self.width as i32).max(other.x + other.width as i32);
        let bottom = (self.y + self.height as i32).max(other.y + other.height as i32);

        Rectangle {
            x: left,
            y: top,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        }
    }

    /// Mover el rectángulo
    pub fn translate(&self, dx: i32, dy: i32) -> Rectangle {
        Rectangle {
            x: self.x + dx,
            y: self.y + dy,
            width: self.width,
            height: self.height,
        }
    }

    /// Redimensionar el rectángulo
    pub fn resize(&self, new_width: u32, new_height: u32) -> Rectangle {
        Rectangle {
            x: self.x,
            y: self.y,
            width: new_width,
            height: new_height,
        }
    }

    /// Redimensionar desde el centro
    pub fn resize_from_center(&self, new_width: u32, new_height: u32) -> Rectangle {
        let center = self.center();
        Rectangle {
            x: center.x - (new_width as i32) / 2,
            y: center.y - (new_height as i32) / 2,
            width: new_width,
            height: new_height,
        }
    }

    /// Clampar el rectángulo dentro de otro rectángulo
    pub fn clamp_to(&self, bounds: &Rectangle) -> Rectangle {
        let mut result = *self;

        if result.x < bounds.x {
            result.width = result.width.saturating_sub((bounds.x - result.x) as u32);
            result.x = bounds.x;
        }

        if result.y < bounds.y {
            result.height = result.height.saturating_sub((bounds.y - result.y) as u32);
            result.y = bounds.y;
        }

        let max_right = bounds.x + bounds.width as i32;
        let max_bottom = bounds.y + bounds.height as i32;
        let result_right = result.x + result.width as i32;
        let result_bottom = result.y + result.height as i32;

        if result_right > max_right {
            result.width = (max_right - result.x) as u32;
        }

        if result_bottom > max_bottom {
            result.height = (max_bottom - result.y) as u32;
        }

        result
    }
}

/// Región 2D (conjunto de rectángulos)
#[derive(Debug, Clone)]
pub struct Region {
    rectangles: alloc::vec::Vec<Rectangle>,
}

impl Region {
    pub fn new() -> Self {
        Self {
            rectangles: alloc::vec::Vec::new(),
        }
    }

    /// Crear región desde un rectángulo
    pub fn from_rectangle(rect: Rectangle) -> Self {
        Self {
            rectangles: alloc::vec![rect],
        }
    }

    /// Agregar rectángulo a la región
    pub fn add_rectangle(&mut self, rect: Rectangle) {
        self.rectangles.push(rect);
        self.simplify();
    }

    /// Verificar si un punto está en la región
    pub fn contains_point(&self, point: &Point) -> bool {
        self.rectangles
            .iter()
            .any(|rect| rect.contains_point(point))
    }

    /// Verificar si un rectángulo está completamente en la región
    pub fn contains_rectangle(&self, rect: &Rectangle) -> bool {
        self.rectangles.iter().any(|r| r.contains_rectangle(rect))
    }

    /// Verificar si la región intersecta con un rectángulo
    pub fn intersects_rectangle(&self, rect: &Rectangle) -> bool {
        self.rectangles.iter().any(|r| r.intersects(rect))
    }

    /// Simplificar la región (eliminar rectángulos redundantes)
    fn simplify(&mut self) {
        // Implementación básica: mantener solo rectángulos no solapados
        let mut simplified: alloc::vec::Vec<Rectangle> = alloc::vec::Vec::new();

        for rect in &self.rectangles {
            let mut should_add = true;

            for existing in &simplified {
                if existing.contains_rectangle(rect) {
                    should_add = false;
                    break;
                }
            }

            if should_add {
                simplified.push(*rect);
            }
        }

        self.rectangles = simplified;
    }

    /// Obtener todos los rectángulos de la región
    pub fn rectangles(&self) -> &[Rectangle] {
        &self.rectangles
    }

    /// Área total de la región
    pub fn area(&self) -> u32 {
        self.rectangles.iter().map(|rect| rect.area()).sum()
    }

    /// Verificar si la región está vacía
    pub fn is_empty(&self) -> bool {
        self.rectangles.is_empty()
    }

    /// Limpiar la región
    pub fn clear(&mut self) {
        self.rectangles.clear();
    }
}

impl Default for Region {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_point_operations() {
        let p1 = Point::new(0, 0);
        let p2 = Point::new(3, 4);

        assert_eq!(p1.magnitude(), 0.0);
        assert_eq!(p2.magnitude(), 5.0);
        assert_eq!(p1.distance_to(&p2), 5.0);
    }

    #[test]
    fn test_rectangle_operations() {
        let rect1 = Rectangle::new(0, 0, 100, 100);
        let rect2 = Rectangle::new(50, 50, 100, 100);
        let point = Point::new(25, 25);

        assert!(rect1.contains_point(&point));
        assert!(rect1.intersects(&rect2));

        let intersection = rect1.intersection(&rect2);
        assert!(intersection.is_some());
        assert_eq!(intersection.unwrap(), Rectangle::new(50, 50, 50, 50));
    }
}
