#[cfg(target_os = "android")]
#[macro_use]
extern crate android_glue;

extern crate glutin;

mod support;

#[cfg(target_os = "android")]
android_start!(main);

fn resize_callback(width: u32, height: u32) {
    println!("Window resized to {}x{}", width, height);
}

fn main() {
    println!("begin building window\n");

    let mut window = glutin::WindowBuilder::new().build().unwrap();
    window.set_title("A fantastic window!");
    window.set_window_resize_callback(Some(resize_callback as fn(u32, u32)));
    let _ = unsafe { window.make_current() };
    let context = support::load(&window);

    println!("\nbegin loop\n");
    for event in window.wait_events() {
        context.draw_frame((0.0, 1.0, 0.0, 1.0));
        let _ = window.swap_buffers();

        match event {
            glutin::Event::Closed => {
                println!("\nbreak loop via `Closed`\n");
                break
            },
            glutin::Event::KeyboardInput(glutin::ElementState::Pressed, 53, Some(glutin::VirtualKeyCode::Escape)) => {
                println!("\nbreak loop via `Escape`\n");
                break
            },
            _ => ()
        }
    }

}
