//! Icône de Viherd dessinée en code (pas de fichier image) : pastille verte avec un « V » blanc.
//! Partagée par l'appli de bureau et l'installeur.

/// Pixels RGBA d'une icône carrée de `n` pixels.
pub fn rgba(n: u32) -> (Vec<u8>, u32, u32) {
    let s = n as f32 / 32.0; // le dessin est pensé en 32×32
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = ((x as f32 + 0.5) / s - 16.0, (y as f32 + 0.5) / s - 16.0);
            let inside = fx * fx + fy * fy <= 15.5 * 15.5;
            // deux segments du V : (9,9)->(16,24) et (23,9)->(16,24)
            let v = seg_dist(fx + 16.0, fy + 16.0, 9.5, 9.0, 16.0, 24.0) < 2.2
                || seg_dist(fx + 16.0, fy + 16.0, 22.5, 9.0, 16.0, 24.0) < 2.2;
            let rgba = match (inside, v) {
                (false, _) => [0, 0, 0, 0],
                (true, true) => [255, 255, 255, 255],
                (true, false) => [47, 111, 78, 255],
            };
            px.extend_from_slice(&rgba);
        }
    }
    (px, n, n)
}

fn seg_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
}
