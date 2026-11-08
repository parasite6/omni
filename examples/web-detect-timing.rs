//! Measure external-browser metadata lookup without opening the launcher.
#[path="../src/web.rs"]
mod web;
fn main() {
 for pass in 1..=5 {
  let started=std::time::Instant::now();
  let _=web::Engine::detect();
  println!("Detection {pass}: {:.1} ms",started.elapsed().as_secs_f64()*1000.0);
 }
}
