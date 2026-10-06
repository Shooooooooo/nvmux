//! What flies off the cursor as it moves: Neovide's `vfx_mode`, in a
//! terminal.
//!
//! Three of its modes are trails of particles left along the way the cursor
//! went: `railgun` coils them off the path in a spiral, `torpedo` sprays them
//! out behind it, `pixiedust` lets them drift down like dust. A particle is a
//! braille dot — eight to a cell, so they fly at a quarter of a cell's height
//! — drawn in the cursor's colour and fading with its life. A dot is drawn
//! only in a blank cell: over text, where a dot would take the character's
//! place, the particle is a glow in the cell's background instead, and the
//! text stays readable through it.
//!
//! The other three mark where the cursor landed: `sonicboom` a disc that
//! grows and fades, `ripple` a ring, `wireframe` a square outline. They are
//! drawn as glows too — a tint of the background in the cells they cover — so
//! they never hide a character either.
//!
//! Positions are in cells, but a particle moves in a space where a row is as
//! tall as two columns are wide ([`ASPECT`]), so that a spiral is round and a
//! spray is even on a screen whose cells are not square.

use super::raster;
use super::smear::ASPECT;
use crate::client::compose::Frame;
use crate::client::grid::Text;
use crate::client::style::{mix, Color, Colors};
use crate::palette::Rgb;

/// Neovide's six modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Railgun,
    Torpedo,
    PixieDust,
    SonicBoom,
    Ripple,
    Wireframe,
}

impl Mode {
    /// The mode `[effects.particles] mode` names, spelt as Neovide spells it.
    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "railgun" => Mode::Railgun,
            "torpedo" => Mode::Torpedo,
            "pixiedust" => Mode::PixieDust,
            "sonicboom" => Mode::SonicBoom,
            "ripple" => Mode::Ripple,
            "wireframe" => Mode::Wireframe,
            _ => return None,
        })
    }

    /// The modes' names, for a config error that has to say what it takes.
    pub const NAMES: &'static str = "railgun, torpedo, pixiedust, sonicboom, ripple or wireframe";

    fn is_trail(self) -> bool {
        matches!(self, Mode::Railgun | Mode::Torpedo | Mode::PixieDust)
    }
}

/// `[effects.particles]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub mode: Mode,
    /// How strongly a particle shows at its brightest, 0 to 1.
    pub opacity: f32,
    /// Seconds a particle lives.
    pub lifetime: f32,
    /// Particles for every cell the cursor travels.
    pub density: f32,
    /// How fast a particle flies, in cells a second.
    pub speed: f32,
}

/// How many turns a railgun's spiral takes over every row the cursor
/// travels: Neovide's `vfx_particle_phase`.
const PHASE: f32 = 1.5;

/// How fast a particle's path bends, in radians a second: Neovide's
/// `vfx_particle_curl`.
const CURL: f32 = 1.0;

/// The most particles in the air at once: a cursor thrown across a large
/// screen should not cost a frame.
const MAX_PARTICLES: usize = 400;

/// How long a mark where the cursor landed lasts, in seconds.
const MARK: f32 = 0.25;

/// How wide a mark grows, in rows across.
const MARK_SIZE: f32 = 3.0;

/// How far over text a particle's glow goes towards its colour, against the
/// dot it would have been in a blank cell.
const GLOW: f32 = 0.45;

#[derive(Debug, Clone, Copy)]
struct Particle {
    /// Columns along, rows down — in the square space: `y` is rows times
    /// [`ASPECT`].
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    lifetime: f32,
}

/// A mark where the cursor landed, `t` from 0 to 1 through its life.
#[derive(Debug, Clone, Copy)]
struct Mark {
    x: f32,
    y: f32,
    t: f32,
}

/// Everything flying off the cursor.
#[derive(Debug)]
pub struct Vfx {
    particles: Vec<Particle>,
    mark: Option<Mark>,
    rng: Rng,
}

impl Default for Vfx {
    fn default() -> Self {
        Self {
            particles: Vec::new(),
            mark: None,
            rng: Rng(getrandom::u64().unwrap_or(0x9e37_79b9_7f4a_7c15) | 1),
        }
    }
}

impl Vfx {
    /// The cursor went from the middle of one cell to the middle of another
    /// (`x` along and `y` down, in cells).
    pub fn moved(&mut self, from: (f32, f32), to: (f32, f32), s: &Settings) {
        if !s.mode.is_trail() {
            self.mark = Some(Mark {
                x: to.0,
                y: to.1 * ASPECT,
                t: 0.0,
            });
            return;
        }
        let (fx, fy) = (from.0, from.1 * ASPECT);
        let (dx, dy) = (to.0 - from.0, (to.1 - from.1) * ASPECT);
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < 0.5 {
            return;
        }
        let room = MAX_PARTICLES.saturating_sub(self.particles.len());
        let count = ((dist * s.density).ceil() as usize).min(room);
        let (ux, uy) = (dx / dist, dy / dist);
        for i in 0..count {
            let along = i as f32 / count as f32;
            let (x, y, vx, vy) = match s.mode {
                Mode::Railgun => {
                    let phase = along * std::f32::consts::TAU * PHASE * (dist / ASPECT).sqrt();
                    (
                        fx + dx * along,
                        fy + dy * along,
                        phase.sin() * s.speed,
                        phase.cos() * s.speed,
                    )
                }
                Mode::Torpedo => {
                    let (rx, ry) = self.rng.direction();
                    let (px, py) = (rx - ux * 1.5, ry - uy * 1.5);
                    let n = (px * px + py * py).sqrt().max(1e-3);
                    let at = self.rng.unit();
                    (
                        fx + dx * at,
                        fy + dy * at,
                        px / n * s.speed,
                        py / n * s.speed,
                    )
                }
                _ => {
                    let (rx, ry) = self.rng.direction();
                    let at = self.rng.unit();
                    (
                        fx + dx * at,
                        fy + dy * at,
                        rx * 0.5 * s.speed * 0.6,
                        (0.4 + ry.abs()) * s.speed * 0.6,
                    )
                }
            };
            // A little each way, so particles born together do not all die
            // together.
            let lifetime = s.lifetime * (0.75 + 0.5 * self.rng.unit());
            self.particles.push(Particle {
                x,
                y,
                vx,
                vy,
                life: lifetime,
                lifetime,
            });
        }
    }

    /// Move on `dt` seconds. Says whether anything is still flying.
    pub fn step(&mut self, dt: f32) -> bool {
        self.particles.retain_mut(|p| {
            p.life -= dt;
            if p.life <= 0.0 {
                return false;
            }
            p.x += p.vx * dt;
            p.y += p.vy * dt;
            let (sin, cos) = (dt * CURL).sin_cos();
            (p.vx, p.vy) = (p.vx * cos - p.vy * sin, p.vx * sin + p.vy * cos);
            true
        });
        if let Some(m) = &mut self.mark {
            m.t += dt / MARK;
            if m.t >= 1.0 {
                self.mark = None;
            }
        }
        self.moving()
    }

    pub fn moving(&self) -> bool {
        !self.particles.is_empty() || self.mark.is_some()
    }

    pub fn clear(&mut self) {
        self.particles.clear();
        self.mark = None;
    }

    /// Draw everything flying over `frame`, in `color`.
    pub fn paint(&self, frame: &mut Frame, colors: &Colors, color: Rgb, s: &Settings) {
        if let Some(m) = self.mark {
            paint_mark(frame, colors, color, s, m);
        }
        // Gather the particles by cell: the dots they light, and the
        // strongest of them.
        let mut cells: std::collections::HashMap<(usize, usize), (u8, f32)> =
            std::collections::HashMap::new();
        for p in &self.particles {
            let row = p.y / ASPECT;
            if p.x < 0.0 || row < 0.0 {
                continue;
            }
            let (r, c) = (row as usize, p.x as usize);
            if r >= frame.height || c >= frame.width {
                continue;
            }
            let dot = raster::braille_dot(p.x.fract(), row.fract());
            let alpha = (p.life / p.lifetime).clamp(0.0, 1.0) * s.opacity;
            let e = cells.entry((r, c)).or_insert((0, 0.0));
            e.0 |= dot;
            e.1 = e.1.max(alpha);
        }
        for ((r, c), (dots, alpha)) in cells {
            let Some(cell) = frame.get_mut(r, c) else {
                continue;
            };
            let bg = colors.visual_bg(&cell.style);
            let pct = |a: f32| (a.clamp(0.0, 1.0) * 100.0) as u8;
            if cell.text.is_blank() && !cell.wide {
                cell.text = Text::Char(raster::braille(dots));
                cell.style.fg = Color::Rgb(mix(pct(alpha), color, bg));
                cell.style.bg = Color::Rgb(bg);
                cell.style.reverse = false;
            } else if cell.text != Text::Half {
                glow(colors, cell, color, alpha * GLOW);
            }
        }
    }
}

/// Tint a cell's background `amount` of the way to `color`, its text kept.
fn glow(colors: &Colors, cell: &mut crate::client::compose::Out, color: Rgb, amount: f32) {
    let fg = colors.visual_fg(&cell.style);
    let bg = colors.visual_bg(&cell.style);
    let pct = (amount.clamp(0.0, 1.0) * 100.0) as u8;
    cell.style.fg = Color::Rgb(fg);
    cell.style.bg = Color::Rgb(mix(pct, color, bg));
    cell.style.reverse = false;
}

/// A disc, a ring or a square around where the cursor landed, as a glow.
fn paint_mark(frame: &mut Frame, colors: &Colors, color: Rgb, s: &Settings, m: Mark) {
    // Grows from nothing to its full size, fading as it does — fast at
    // first, as Neovide eases it.
    let radius = m.t * MARK_SIZE * ASPECT / 2.0;
    let alpha = s.opacity * (1.0 - m.t * m.t);
    if radius <= 0.0 || alpha <= 0.0 {
        return;
    }
    let stroke = 1.0f32.max(0.2 * ASPECT);
    let reach = radius + stroke;
    let rows = ((m.y - reach) / ASPECT).floor() as i64..((m.y + reach) / ASPECT).ceil() as i64 + 1;
    let cols = (m.x - reach).floor() as i64..(m.x + reach).ceil() as i64 + 1;
    for r in rows {
        for c in cols.clone() {
            let (Ok(ru), Ok(cu)) = (usize::try_from(r), usize::try_from(c)) else {
                continue;
            };
            // Four by four samples: how much of the cell the shape covers.
            let mut hit = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = c as f32 + (sx as f32 + 0.5) / 4.0 - m.x;
                    let y = (r as f32 + (sy as f32 + 0.5) / 4.0) * ASPECT - m.y;
                    let on = match s.mode {
                        Mode::SonicBoom => (x * x + y * y).sqrt() <= radius,
                        Mode::Ripple => ((x * x + y * y).sqrt() - radius).abs() <= stroke / 2.0,
                        _ => {
                            let edge = x.abs().max(y.abs());
                            (edge - radius).abs() <= stroke / 2.0
                        }
                    };
                    hit += usize::from(on);
                }
            }
            if hit == 0 {
                continue;
            }
            if let Some(cell) = frame.get_mut(ru, cu) {
                if cell.text != Text::Half {
                    glow(colors, cell, color, alpha * hit as f32 / 16.0);
                }
            }
        }
    }
}

/// A small generator: nothing depends on it but the look of the particles.
#[derive(Debug)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `0..1`.
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A direction, uniform around the circle.
    fn direction(&mut self) -> (f32, f32) {
        let a = self.unit() * std::f32::consts::TAU;
        (a.cos(), a.sin())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::style::{DefaultColors, Style};
    use crate::palette::Palette;

    fn settings(mode: Mode) -> Settings {
        Settings {
            mode,
            opacity: 0.8,
            lifetime: 0.5,
            density: 2.0,
            speed: 6.0,
        }
    }

    fn colors() -> Colors {
        Colors {
            rgb: true,
            defaults: DefaultColors::default(),
            term: Palette {
                fg: Rgb(255, 255, 255),
                bg: Rgb(0, 0, 0),
                ansi: [Rgb(0, 0, 0); 16],
            },
        }
    }

    #[test]
    fn modes_are_named_as_neovide_names_them() {
        for name in [
            "railgun",
            "torpedo",
            "pixiedust",
            "sonicboom",
            "ripple",
            "wireframe",
        ] {
            assert!(Mode::named(name).is_some(), "{name}");
            assert!(Mode::NAMES.contains(name));
        }
        assert_eq!(Mode::named("sparkles"), None);
    }

    /// A trail is laid along the way the cursor went, more for further, and
    /// it dies away.
    #[test]
    fn a_trail_is_laid_along_the_way_and_dies_away() {
        for mode in [Mode::Railgun, Mode::Torpedo, Mode::PixieDust] {
            let mut v = Vfx::default();
            v.moved((0.5, 0.5), (20.5, 0.5), &settings(mode));
            let far = v.particles.len();
            assert!(far >= 30, "{mode:?}: {far}");
            let mut near = Vfx::default();
            near.moved((0.5, 0.5), (5.5, 0.5), &settings(mode));
            assert!(near.particles.len() < far);
            let mut t = 0.0;
            while v.step(0.016) {
                t += 0.016;
                assert!(t < 1.0, "{mode:?} never died away");
            }
        }
    }

    /// No particle more than the ceiling, however far the cursor is thrown.
    #[test]
    fn the_air_holds_only_so_many() {
        let mut v = Vfx::default();
        for _ in 0..10 {
            v.moved((0.0, 0.0), (300.0, 80.0), &settings(Mode::Railgun));
        }
        assert!(v.particles.len() <= MAX_PARTICLES);
    }

    /// A particle over a blank is a dot; over text it is a glow, and the
    /// text stays.
    #[test]
    fn a_dot_in_a_blank_a_glow_over_text() {
        let mut frame = Frame::new(4, 1, Style::default());
        frame.cells[1].text = Text::Char('x');
        let mut v = Vfx::default();
        for x in [0.3, 1.3] {
            v.particles.push(Particle {
                x,
                y: 0.2 * ASPECT,
                vx: 0.0,
                vy: 0.0,
                life: 0.5,
                lifetime: 0.5,
            });
        }
        v.paint(
            &mut frame,
            &colors(),
            Rgb(255, 0, 0),
            &settings(Mode::Railgun),
        );
        assert_eq!(frame.cells[0].text, Text::Char('⠁'));
        assert_eq!(frame.cells[1].text, Text::Char('x'));
        assert_ne!(frame.cells[1].style.bg, Style::default().bg, "glowing");
        assert_eq!(frame.cells[2].text, Text::Char(' '));
    }

    /// A mark grows round where the cursor landed and goes; it hides no text.
    #[test]
    fn a_mark_grows_and_goes_and_hides_nothing() {
        for mode in [Mode::SonicBoom, Mode::Ripple, Mode::Wireframe] {
            let mut v = Vfx::default();
            v.moved((0.0, 0.0), (10.5, 5.5), &settings(mode));
            v.step(MARK * 0.5);
            let mut frame = Frame::new(21, 11, Style::default());
            for cell in &mut frame.cells {
                cell.text = Text::Char('x');
            }
            v.paint(&mut frame, &colors(), Rgb(0, 255, 0), &settings(mode));
            let lit = frame
                .cells
                .iter()
                .filter(|c| c.style.bg != Style::default().bg)
                .count();
            assert!(lit > 0, "{mode:?} drew nothing");
            assert!(frame.cells.iter().all(|c| c.text == Text::Char('x')));
            assert!(!v.step(MARK), "{mode:?} outlived itself");
        }
    }
}
