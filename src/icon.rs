//! Icône de Viherd dessinée en code (pas de fichier image) : pastille verte avec un « V » blanc.
//! Partagée par l'appli de bureau, l'installeur et les scripts de build (fichier .ico des .exe) :
//! ce module ne dépend d'aucune crate.

/// Pixels RGBA d'une icône carrée de `n` pixels (bords lissés par suréchantillonnage 4×4).
pub fn rgba(n: u32) -> (Vec<u8>, u32, u32) {
    const SS: u32 = 4;
    let s = n as f32 / 32.0; // le dessin est pensé en 32×32
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            // moyenne prémultipliée des sous-échantillons
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = (x as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let fy = (y as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    let [cr, cg, cb, ca] = sample(fx, fy);
                    r += cr * ca;
                    g += cg * ca;
                    b += cb * ca;
                    a += ca;
                }
            }
            let k = (SS * SS) as f32;
            let out = if a == 0.0 { [0, 0, 0, 0] } else {
                [(r / a) as u8, (g / a) as u8, (b / a) as u8, (a / k * 255.0).round() as u8]
            };
            px.extend_from_slice(&out);
        }
    }
    (px, n, n)
}

/// Couleur du dessin en un point (coordonnées 0..32) : RGB 0-255, alpha 0-1.
fn sample(x: f32, y: f32) -> [f32; 4] {
    let (dx, dy) = (x - 16.0, y - 16.0);
    if dx * dx + dy * dy > 15.5 * 15.5 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    // deux segments du V : (9.5,9)->(16,24) et (22.5,9)->(16,24)
    let v = seg_dist(x, y, 9.5, 9.0, 16.0, 24.0) < 2.2 || seg_dist(x, y, 22.5, 9.0, 16.0, 24.0) < 2.2;
    if v { [255.0, 255.0, 255.0, 1.0] } else { [47.0, 111.0, 78.0, 1.0] }
}

fn seg_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
}

/// Fichier .ico (images BMP 32 bits) aux tailles utilisées par Windows : barre des tâches, Explorateur, raccourcis.
pub fn ico() -> Vec<u8> {
    const SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&n| bmp(n)).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]); // réservé, type 1 = icône
    out.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (n, img) in SIZES.iter().zip(&images) {
        let side = if *n >= 256 { 0 } else { *n as u8 }; // 0 signifie 256
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // plans
        out.extend_from_slice(&32u16.to_le_bytes()); // bits par pixel
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

/// Une image de l'.ico : en-tête BITMAPINFOHEADER, pixels BGRA de bas en haut, masque AND vide.
fn bmp(n: u32) -> Vec<u8> {
    let (px, _, _) = rgba(n);
    let mask_row = n.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + (n * n * 4 + mask_row * n) as usize);
    for v in [40u32, n, n * 2] { out.extend_from_slice(&v.to_le_bytes()); } // taille, largeur, hauteur (×2 : masque inclus)
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for v in [0u32, n * n * 4 + mask_row * n, 0, 0, 0, 0] { out.extend_from_slice(&v.to_le_bytes()); }
    for y in (0..n).rev() {
        for x in 0..n {
            let i = ((y * n + x) * 4) as usize;
            out.extend_from_slice(&[px[i + 2], px[i + 1], px[i], px[i + 3]]);
        }
    }
    out.resize(out.len() + (mask_row * n) as usize, 0);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn ico_has_a_valid_directory() {
        let ico = super::ico();
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        // la dernière image se termine exactement en fin de fichier
        let last = 6 + 16 * (count - 1);
        let size = u32::from_le_bytes(ico[last + 8..last + 12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(ico[last + 12..last + 16].try_into().unwrap()) as usize;
        assert_eq!(offset + size, ico.len());
        // bord transparent, centre opaque
        let (px, n, _) = super::rgba(32);
        assert_eq!(px[3], 0);
        assert_eq!(px[((n / 2 * n + n / 2) * 4 + 3) as usize], 255);
    }
}
