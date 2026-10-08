//! Native launcher for weft-room-scope. See `weftos_room_scope::USAGE`.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    match weftos_room_scope::Opts::parse(std::env::args().skip(1)) {
        Ok(o) => weftos_room_scope::run_native(o),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
