//! `lights`: the keyboard lights of a Clevo laptop, on the `pc` board.

use core::fmt::{self, Write};
use green_tea_bare::Board;
use green_tea_bare::clevo_lights::{Color, Error};

pub const HELP: &str = "\
lights             whether this machine's keyboard lights can be set
lights <color>     the keyboard's color: a name (red, orange, ...) or rrggbb
lights bright <n>  brightness, 0 to 255
lights off         turn them off
";

pub fn command(args: &[&str], out: &mut impl Write) -> fmt::Result {
    let result = match args {
        [] => Board::keyboard_lights().map(|_| "the keyboard lights are ready"),
        ["off"] => with_lights(|l| l.set_brightness(0)).map(|()| "off"),
        ["bright", n] => match n.parse() {
            Ok(n) => with_lights(|l| l.set_brightness(n)).map(|()| "brightness set"),
            Err(_) => return writeln!(out, "usage: lights bright <0 to 255>"),
        },
        [color] => {
            let Some(parsed) = Color::parse(color) else { return unknown(out, color) };
            with_lights(|l| l.set_brightness(255).and_then(|()| l.set_color(parsed))).map(|()| "color set")
        }
        _ => return out.write_str(HELP),
    };
    match result {
        Ok(message) => writeln!(out, "lights: {message}"),
        Err(e) => writeln!(out, "lights: {e}"),
    }
}

type Lights = green_tea_bare::clevo_lights::Lights<green_tea_bare::EcPort>;

fn with_lights(f: impl FnOnce(&mut Lights) -> Result<(), Error>) -> Result<(), Error> {
    f(&mut Board::keyboard_lights()?)
}

fn unknown(out: &mut impl Write, color: &str) -> fmt::Result {
    writeln!(out, "unknown color `{color}`: try red, green, blue, white, orange, yellow, cyan, purple, pink, or rrggbb")
}
