const ICON_SIZE: u32 = 64;

/// Returns `(rgba_data, size)` for a circle-with-arrows tray icon.
/// Use `filled=false` for an outline ring, `filled=true` for a solid circle.
pub fn make_icon_rgba(r: u8, g: u8, b: u8, filled: bool) -> (Vec<u8>, u32) {
    let sz = ICON_SIZE;
    let mut rgba = vec![0u8; (sz * sz * 4) as usize];
    let cx = sz as f32 / 2.0;
    let cy = cx;
    let outer_r = cx - 1.0;
    let ring_w: f32 = 3.0;

    for y in 0..sz {
        for x in 0..sz {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let a = if filled {
                (outer_r - d + 0.5).clamp(0.0, 1.0)
            } else {
                let outer = (outer_r - d + 0.5).clamp(0.0, 1.0);
                let inner = (d - (outer_r - ring_w) + 0.5).clamp(0.0, 1.0);
                outer * inner
            };
            if a > 0.0 {
                let i = ((y * sz + x) * 4) as usize;
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = (a * 255.0) as u8;
            }
        }
    }

    let fg: [u8; 4] = if filled { [255, 255, 255, 255] } else { [r, g, b, 255] };
    draw_arrows(&mut rgba, sz, &fg);

    (rgba, sz)
}

fn draw_arrows(rgba: &mut [u8], sz: u32, color: &[u8; 4]) {
    let cx = sz as f32 / 2.0;
    let cy = cx;
    let s = sz as f32 / 128.0;

    let set_px = |rgba: &mut [u8], x: i32, y: i32, a: f32| {
        if x < 0 || y < 0 || x >= sz as i32 || y >= sz as i32 { return; }
        let i = ((y as u32 * sz + x as u32) * 4) as usize;
        let sa = a * (color[3] as f32 / 255.0);
        let da = rgba[i + 3] as f32 / 255.0;
        let oa = sa + da * (1.0 - sa);
        if oa > 0.0 {
            rgba[i]     = ((color[0] as f32 * sa + rgba[i]     as f32 * da * (1.0 - sa)) / oa) as u8;
            rgba[i + 1] = ((color[1] as f32 * sa + rgba[i + 1] as f32 * da * (1.0 - sa)) / oa) as u8;
            rgba[i + 2] = ((color[2] as f32 * sa + rgba[i + 2] as f32 * da * (1.0 - sa)) / oa) as u8;
            rgba[i + 3] = (oa * 255.0) as u8;
        }
    };

    let fill_rect = |rgba: &mut [u8], x1: f32, y1: f32, x2: f32, y2: f32| {
        let ix1 = x1.floor() as i32;
        let iy1 = y1.floor() as i32;
        let ix2 = x2.ceil() as i32;
        let iy2 = y2.ceil() as i32;
        for py in iy1..iy2 {
            for px in ix1..ix2 {
                let ox = ((px as f32 + 0.5) - x1).clamp(0.0, 1.0) * (x2 - (px as f32 + 0.5)).clamp(0.0, 1.0);
                let oy = ((py as f32 + 0.5) - y1).clamp(0.0, 1.0) * (y2 - (py as f32 + 0.5)).clamp(0.0, 1.0);
                let cov = ox * oy;
                if cov > 0.0 { set_px(rgba, px, py, cov); }
            }
        }
    };

    let fill_tri = |rgba: &mut [u8], pts: [(f32, f32); 3]| {
        let min_x = pts.iter().map(|p| p.0).fold(f32::MAX, f32::min).floor() as i32;
        let max_x = pts.iter().map(|p| p.0).fold(f32::MIN, f32::max).ceil() as i32;
        let min_y = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i32;
        let max_y = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i32;
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                let d0 = (pts[1].0 - pts[0].0) * (fy - pts[0].1) - (pts[1].1 - pts[0].1) * (fx - pts[0].0);
                let d1 = (pts[2].0 - pts[1].0) * (fy - pts[1].1) - (pts[2].1 - pts[1].1) * (fx - pts[1].0);
                let d2 = (pts[0].0 - pts[2].0) * (fy - pts[2].1) - (pts[0].1 - pts[2].1) * (fx - pts[2].0);
                let inside = (d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0) || (d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0);
                if inside { set_px(rgba, px, py, 1.0); }
            }
        }
    };

    let w = 6.0 * s;
    let ty = cy - 14.0 * s;
    fill_rect(rgba, cx - 30.0 * s, ty - w / 2.0, cx + 15.0 * s, ty + w / 2.0);
    fill_tri(rgba, [
        (cx + 30.0 * s, ty),
        (cx + 12.0 * s, ty - 14.0 * s),
        (cx + 12.0 * s, ty + 14.0 * s),
    ]);
    let by = cy + 14.0 * s;
    fill_rect(rgba, cx - 15.0 * s, by - w / 2.0, cx + 30.0 * s, by + w / 2.0);
    fill_tri(rgba, [
        (cx - 30.0 * s, by),
        (cx - 12.0 * s, by - 14.0 * s),
        (cx - 12.0 * s, by + 14.0 * s),
    ]);
}
