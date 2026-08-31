//! Rastreamento de posição virtual do cursor por deltas relativos (`REL_X`/
//! `REL_Y`) e detecção de cruzamento de borda da tela configurada.

/// Direção em que o cursor cruzou a borda da tela.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Acumula deltas relativos de mouse dentro dos limites de uma tela de
/// `width` x `height` e sinaliza quando a posição virtual cruza uma borda.
pub struct CursorTracker {
    x: i64,
    y: i64,
    width: u32,
    height: u32,
}

impl CursorTracker {
    /// Começa no centro da tela: é a posição mais distante de qualquer
    /// borda, então o cursor não dispara uma transição espúria assim que o
    /// tracker é criado.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            x: i64::from(width / 2),
            y: i64::from(height / 2),
            width,
            height,
        }
    }

    /// Acumula o delta e retorna a direção cruzada, se alguma. Ao cruzar,
    /// a posição na direção cruzada é resetada para o centro dessa mesma
    /// dimensão — origem razoável para "entrar" na tela vizinha, sem
    /// depender da posição de saída.
    pub fn accumulate(&mut self, dx: i32, dy: i32) -> Option<Direction> {
        self.x += i64::from(dx);
        self.y += i64::from(dy);

        if self.x < 0 {
            self.x = i64::from(self.width / 2);
            return Some(Direction::Left);
        }
        if self.x >= i64::from(self.width) {
            self.x = i64::from(self.width / 2);
            return Some(Direction::Right);
        }
        if self.y < 0 {
            self.y = i64::from(self.height / 2);
            return Some(Direction::Up);
        }
        if self.y >= i64::from(self.height) {
            self.y = i64::from(self.height / 2);
            return Some(Direction::Down);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_within_bounds_returns_none() {
        let mut tracker = CursorTracker::new(1920, 1080);
        assert_eq!(tracker.accumulate(10, 10), None);
        assert_eq!(tracker.accumulate(-5, -5), None);
    }

    #[test]
    fn crosses_right_edge() {
        let mut tracker = CursorTracker::new(1920, 1080);
        assert_eq!(tracker.accumulate(2000, 0), Some(Direction::Right));
    }

    #[test]
    fn crosses_left_edge() {
        let mut tracker = CursorTracker::new(1920, 1080);
        assert_eq!(tracker.accumulate(-2000, 0), Some(Direction::Left));
    }

    #[test]
    fn crosses_down_edge() {
        let mut tracker = CursorTracker::new(1920, 1080);
        assert_eq!(tracker.accumulate(0, 2000), Some(Direction::Down));
    }

    #[test]
    fn crosses_up_edge() {
        let mut tracker = CursorTracker::new(1920, 1080);
        assert_eq!(tracker.accumulate(0, -2000), Some(Direction::Up));
    }
}
