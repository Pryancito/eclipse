#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};
use eclipse_libc::{println, yield_cpu};
use sidewind_sdk::{discover_composer, SideWindSurface};
use sidewind_opengl::{Mat4, GL_COLOR_BUFFER_BIT, GL_DEPTH_BUFFER_BIT, Pipeline};

const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4MB for demo
#[repr(align(4096))]
struct Heap([u8; HEAP_SIZE]);
static mut HEAP: Heap = Heap([0u8; HEAP_SIZE]);
static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

struct StaticAllocator;
unsafe impl GlobalAlloc for StaticAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();
        let current = HEAP_PTR.load(Ordering::SeqCst);
        let aligned = (current + align - 1) & !(align - 1);
        if aligned + size > HEAP_SIZE { return core::ptr::null_mut(); }
        HEAP_PTR.store(aligned + size, Ordering::SeqCst);
        HEAP.0.as_mut_ptr().add(aligned)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: StaticAllocator = StaticAllocator;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("[GL-DEMO] Starting OpenGL Demo Client...");

    let composer_pid = loop {
        if let Some(pid) = discover_composer() {
            println!("[GL-DEMO] Discovered compositor at PID {}", pid);
            break pid;
        }
        yield_cpu();
    };

    let mut surface = match SideWindSurface::new(composer_pid, 100, 100, 640, 480, "gl_demo") {
        Some(s) => s,
        None => {
            println!("[GL-DEMO] Failed to create surface, idling");
            loop { yield_cpu(); }
        }
    };

    println!("[GL-DEMO] Surface created. Initializing OpenGL context.");

    let mut gl = surface.gl_context();
    gl.set_clear_color(0.01, 0.01, 0.03, 1.0);
    gl.enable_depth_test(true);

    // Cube vertices: [x, y, z, w, u, v, nx, ny, nz, r, g, b, a]
    // 24 vertices (6 faces * 4 verts) to have proper face normals
    let mut vertices = [0.0f32; 24 * 13];
    let mut v_idx = 0;

    let add_face = |v: &mut [f32], idx: &mut usize, p: [[f32; 3]; 4], n: [f32; 3], c: [f32; 3]| {
        for i in 0..4 {
            v[*idx + 0] = p[i][0]; v[*idx + 1] = p[i][1]; v[*idx + 2] = p[i][2]; v[*idx + 3] = 1.0; // pos
            v[*idx + 4] = if i == 1 || i == 2 { 1.0 } else { 0.0 }; // u
            v[*idx + 5] = if i == 2 || i == 3 { 1.0 } else { 0.0 }; // v
            v[*idx + 6] = n[0]; v[*idx + 7] = n[1]; v[*idx + 8] = n[2]; // normal
            v[*idx + 9] = c[0]; v[*idx + 10] = c[1]; v[*idx + 11] = c[2]; v[*idx + 12] = 1.0; // color
            *idx += 13;
        }
    };

    // Front (Red)
    add_face(&mut vertices, &mut v_idx, [[-0.5, -0.5,  0.5], [ 0.5, -0.5,  0.5], [ 0.5,  0.5,  0.5], [-0.5,  0.5,  0.5]], [0.0, 0.0, 1.0], [1.0, 0.2, 0.2]);
    // Back (Magenta)
    add_face(&mut vertices, &mut v_idx, [[ 0.5, -0.5, -0.5], [-0.5, -0.5, -0.5], [-0.5,  0.5, -0.5], [ 0.5,  0.5, -0.5]], [0.0, 0.0, -1.0], [1.0, 0.2, 1.0]);
    // Top (Yellow)
    add_face(&mut vertices, &mut v_idx, [[-0.5,  0.5,  0.5], [ 0.5,  0.5,  0.5], [ 0.5,  0.5, -0.5], [-0.5,  0.5, -0.5]], [0.0, 1.0, 0.0], [1.0, 1.0, 0.2]);
    // Bottom (White)
    add_face(&mut vertices, &mut v_idx, [[-0.5, -0.5, -0.5], [ 0.5, -0.5, -0.5], [ 0.5, -0.5,  0.5], [-0.5, -0.5,  0.5]], [0.0, -1.0, 0.0], [1.0, 1.0, 1.0]);
    // Right (Green)
    add_face(&mut vertices, &mut v_idx, [[ 0.5, -0.5,  0.5], [ 0.5, -0.5, -0.5], [ 0.5,  0.5, -0.5], [ 0.5,  0.5,  0.5]], [1.0, 0.0, 0.0], [0.2, 1.0, 0.2]);
    // Left (Cyan)
    add_face(&mut vertices, &mut v_idx, [[-0.5, -0.5, -0.5], [-0.5, -0.5,  0.5], [-0.5,  0.5,  0.5], [-0.5,  0.5, -0.5]], [-1.0, 0.0, 0.0], [0.2, 1.0, 1.0]);

    let mut indices = [0u32; 36];
    for f in 0..6 {
        let base = f * 4;
        indices[f as usize * 6 + 0] = base + 0;
        indices[f as usize * 6 + 1] = base + 1;
        indices[f as usize * 6 + 2] = base + 2;
        indices[f as usize * 6 + 3] = base + 2;
        indices[f as usize * 6 + 4] = base + 3;
        indices[f as usize * 6 + 5] = base + 0;
    }

    // Custom Lit Shader
    use sidewind_opengl::{VertexShader, FragmentShader, VertexIn, Varying, Vec3};
    struct LitVS { mvp: Mat4, model: Mat4 }
    impl VertexShader for LitVS {
        fn process(&self, v: VertexIn) -> Varying {
            let pos = v.position();
            let norm = Vec3::new(*v.data.get(6).unwrap_or(&0.0), *v.data.get(7).unwrap_or(&0.0), *v.data.get(8).unwrap_or(&0.0));
            Varying {
                clip_pos: self.mvp.mul_vec4(pos),
                color: [*v.data.get(9).unwrap_or(&1.0), *v.data.get(10).unwrap_or(&1.0), *v.data.get(11).unwrap_or(&1.0), 1.0],
                uv: v.uv(),
                normal: self.model.mul_vec4(norm.to_vec4(0.0)).xyz().normalize(), // world-space normal
            }
        }
    }
    struct LitFS { light_dir: Vec3 }
    impl FragmentShader for LitFS {
        fn process(&self, v: Varying) -> Option<[f32; 4]> {
            let dot = v.normal.dot(self.light_dir).max(0.1); // min 0.1 for ambient
            Some([v.color[0] * dot, v.color[1] * dot, v.color[2] * dot, 1.0])
        }
    }

    let mut angle_x = 0.0f32;
    let mut angle_y = 0.0f32;
    let mut last_mouse_x = 0i32;
    let mut last_mouse_y = 0i32;
    let mut mouse_pressed = false;
    let zoom = -2.5f32;

    loop {
        // Poll events
        while let Some(ev) = surface.poll_event() {
            use sidewind_core::{SWND_EVENT_TYPE_MOUSE_MOVE, SWND_EVENT_TYPE_MOUSE_BUTTON};
            match ev.event_type {
                SWND_EVENT_TYPE_MOUSE_MOVE => {
                    if mouse_pressed {
                        angle_y += (ev.data1 - last_mouse_x) as f32 * 0.01;
                        angle_x += (ev.data2 - last_mouse_y) as f32 * 0.01;
                    }
                    last_mouse_x = ev.data1;
                    last_mouse_y = ev.data2;
                }
                SWND_EVENT_TYPE_MOUSE_BUTTON => {
                    if ev.data1 == 0 { // Left button
                        mouse_pressed = ev.data2 != 0;
                    }
                }
                _ => {}
            }
        }

        gl.clear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);

        let model = Mat4::rotate_y(angle_y) * Mat4::rotate_x(angle_x);
        let view = Mat4::translate(0.0, 0.0, zoom);
        let projection = Mat4::perspective(45.0 * 3.14159 / 180.0, 640.0 / 480.0, 0.1, 100.0);
        let mvp = projection * view * model;

        let pipeline = Pipeline::new(
            LitVS { mvp, model },
            LitFS { light_dir: Vec3::new(0.5, 0.5, 1.0).normalize() },
            13 * 4, // 13 floats * 4 bytes
            0,  // position offset
            4   // position components
        );

        unsafe {
            gl.draw_elements(&pipeline, &vertices, &indices);
        }

        surface.commit();
        
        // Auto-rotate if not pressed
        if !mouse_pressed {
            angle_y += 0.01;
            angle_x += 0.005;
        }

        yield_cpu();
    }
}
