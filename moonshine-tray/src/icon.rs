//! Drawing the tray icon.
//!
//! The icon is a crescent moon whose colour carries the state, rasterised here rather
//! than shipped as image assets: it is a few circles, it needs to exist at whatever size
//! the host asks for, and generating it keeps the four state colours from drifting apart
//! from the ones the tooltip and menu use.

use ksni::Icon;

/// Sizes handed to the tray host, which picks whichever suits its panel.
const SIZES: [i32; 4] = [22, 24, 32, 48];

/// Samples per axis used to antialias the circle edges.
const SUPERSAMPLES: i32 = 4;

/// Centre and radius of the moon's body, as a fraction of the icon size.
const BODY: (f32, f32, f32) = (0.5, 0.5, 0.42);

/// Centre and radius of the disc punched out to carve the crescent.
const NOTCH: (f32, f32, f32) = (0.69, 0.438, 0.39);

/// Render the crescent in `colour` (`0xRRGGBB`) at every size the host might want.
pub fn crescent(colour: u32) -> Vec<Icon> {
	SIZES.iter().map(|&size| render(size, colour)).collect()
}

/// Rasterise a single crescent into an ARGB32 buffer in network byte order.
fn render(size: i32, colour: u32) -> Icon {
	let red = ((colour >> 16) & 0xff) as u8;
	let green = ((colour >> 8) & 0xff) as u8;
	let blue = (colour & 0xff) as u8;

	let mut data = Vec::with_capacity((size * size * 4) as usize);
	for y in 0..size {
		for x in 0..size {
			let alpha = (coverage(x, y, size) * 255.0).round() as u8;
			data.extend_from_slice(&[alpha, red, green, blue]);
		}
	}

	Icon {
		width: size,
		height: size,
		data,
	}
}

/// Fraction of the pixel at (`x`, `y`) that lies inside the crescent.
fn coverage(x: i32, y: i32, size: i32) -> f32 {
	let mut inside = 0;

	for sample_y in 0..SUPERSAMPLES {
		for sample_x in 0..SUPERSAMPLES {
			// Sample at subpixel centres, so the sample grid is symmetric within the pixel.
			let offset = |i: i32| (i as f32 + 0.5) / SUPERSAMPLES as f32;
			let point_x = (x as f32 + offset(sample_x)) / size as f32;
			let point_y = (y as f32 + offset(sample_y)) / size as f32;

			if within(point_x, point_y, BODY) && !within(point_x, point_y, NOTCH) {
				inside += 1;
			}
		}
	}

	inside as f32 / (SUPERSAMPLES * SUPERSAMPLES) as f32
}

/// Whether a point in unit coordinates falls inside the given (centre x, centre y, radius).
fn within(x: f32, y: f32, circle: (f32, f32, f32)) -> bool {
	let (centre_x, centre_y, radius) = circle;
	let (dx, dy) = (x - centre_x, y - centre_y);

	dx * dx + dy * dy <= radius * radius
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Alpha, red, green and blue of the pixel at the given fraction of the icon.
	fn pixel_at(icon: &Icon, x_fraction: f32, y_fraction: f32) -> (u8, u8, u8, u8) {
		let x = (x_fraction * icon.width as f32) as i32;
		let y = (y_fraction * icon.height as f32) as i32;
		let offset = ((y * icon.width + x) * 4) as usize;

		(
			icon.data[offset],
			icon.data[offset + 1],
			icon.data[offset + 2],
			icon.data[offset + 3],
		)
	}

	#[test]
	fn renders_every_requested_size() {
		let icons = crescent(0x3fb950);
		assert_eq!(icons.len(), SIZES.len());

		for (icon, &size) in icons.iter().zip(SIZES.iter()) {
			assert_eq!(icon.width, size);
			assert_eq!(icon.height, size);
			assert_eq!(icon.data.len(), (size * size * 4) as usize);
		}
	}

	#[test]
	fn the_limb_of_the_crescent_is_opaque_and_carries_the_colour() {
		let icon = &crescent(0x3fb950)[3];
		assert_eq!(pixel_at(icon, 0.12, 0.5), (255, 0x3f, 0xb9, 0x50));
	}

	#[test]
	fn the_notch_is_transparent() {
		let icon = &crescent(0x3fb950)[3];
		assert_eq!(pixel_at(icon, NOTCH.0, NOTCH.1).0, 0);
	}

	#[test]
	fn the_corners_are_transparent() {
		let icon = &crescent(0x3fb950)[3];
		assert_eq!(pixel_at(icon, 0.0, 0.0).0, 0);
		assert_eq!(pixel_at(icon, 0.98, 0.98).0, 0);
	}

	#[test]
	fn edges_are_antialiased_rather_than_hard() {
		let icon = &crescent(0x3fb950)[3];
		let partial = icon.data.iter().step_by(4).filter(|&&alpha| alpha > 0 && alpha < 255);
		assert!(partial.count() > 0, "no partially covered pixels, so no antialiasing");
	}
}
