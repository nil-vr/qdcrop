use std::{
    borrow::Cow,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process,
};

use anyhow::{anyhow, Context};
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgb};
use imageproc::{
    definitions::HasBlack,
    geometric_transformations::{Interpolation, Projection},
};
use nalgebra::{ArrayStorage, Matrix};
use rayon::prelude::*;
use webp::Encoder;

/// Find an inverse projection matrix for a rectangle.
///
/// # Arguments
///
/// * `from` - the points of the forward perspective transformed rectangle, provided clockwise from (0, 0) in the top left.
/// * `to` - the width and height of the image after reverse projection.
///
/// # Errors
///
/// An error will be returned if three of the points in `from` form a line.
fn from_control_points(from: [(f32, f32); 4], to: (u32, u32)) -> anyhow::Result<Projection> {
    // imageproc::geometric_transformations::Projection has a from_control_points,
    // but it seems to randomly fail on trivial cases.
    // This is an implementation of the algorithm used by OpenCV with the solver from nalgebra.
    // It is much more reliable.
    let ((x0, y0), (x1, y1), (x2, y2), (x3, y3)) = (
        (from[0].0 as f64, from[0].1 as f64),
        (from[1].0 as f64, from[1].1 as f64),
        (from[2].0 as f64, from[2].1 as f64),
        (from[3].0 as f64, from[3].1 as f64),
    );
    let ((u0, v0), (u1, v1), (u2, v2), (u3, v3)) = (
        (0.0f64, 0.0f64),
        (to.0 as f64, 0.0f64),
        (to.0 as f64, to.1 as f64),
        (0.0f64, to.1 as f64),
    );

    let a = Matrix::from_data(ArrayStorage([
        [x0, x1, x2, x3, 0.0, 0.0, 0.0, 0.0],
        [y0, y1, y2, y3, 0.0, 0.0, 0.0, 0.0],
        [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 0.0, x0, x1, x2, x3],
        [0.0, 0.0, 0.0, 0.0, y0, y1, y2, y3],
        [0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
        [
            -x0 * u0,
            -x1 * u1,
            -x2 * u2,
            -x3 * u3,
            -x0 * v0,
            -x1 * v1,
            -x2 * v2,
            -x3 * v3,
        ],
        [
            -y0 * u0,
            -y1 * u1,
            -y2 * u2,
            -y3 * u3,
            -y0 * v0,
            -y1 * v1,
            -y2 * v2,
            -y3 * v3,
        ],
    ]));
    let b = Matrix::from_data(ArrayStorage([[u0, u1, u2, u3, v0, v1, v2, v3]]));

    let svd = a
        .try_svd(true, true, f64::EPSILON, 1048576)
        .context("SVD failed")?;
    let x = svd
        .solve(&b, 0.125)
        .map_err(|e| anyhow!("Unable to solve for projection: {:?}", e))?;
    let x = x.column(0);

    Ok(Projection::from_matrix([
        x[0] as f32,
        x[1] as f32,
        x[2] as f32,
        x[3] as f32,
        x[4] as f32,
        x[5] as f32,
        x[6] as f32,
        x[7] as f32,
        1.0,
    ])
    .unwrap())
}

/// Find the position of the black pixel closest to a corner of the image.
///
/// # Arguments
///
/// * `threshold` - The image to search.
/// * `flip_x` - `true` if the search should start from the right.
/// * `flip_y` - `true` if the search should start from the bottom.
fn find_nearest_to_corner<Image: GenericImageView<Pixel = P>, P: HasBlack + PartialEq>(
    threshold: &Image,
    flip_x: bool,
    flip_y: bool,
) -> Option<(u32, u32)> {
    #[derive(Debug)]
    struct Nearest {
        square_distance: usize,
        x: u32,
        y: u32,
    }
    let mut nearest = None;
    for i in 0..std::cmp::max(threshold.width(), threshold.height()) {
        let i_squared = i as usize * i as usize;
        match &nearest {
            Some(Nearest {
                square_distance, ..
            }) if *square_distance < i_squared => break,
            _ => {}
        }

        if i < threshold.height() {
            let real_y = if flip_y {
                threshold.height() - 1 - i
            } else {
                i
            };
            for x in 0..std::cmp::min(i + 1, threshold.width()) {
                let real_x = if flip_x { threshold.width() - 1 - x } else { x };
                if threshold.get_pixel(real_x, real_y) == P::black() {
                    let square_distance = x as usize * x as usize + i_squared;
                    nearest = Some(match nearest {
                        Some(
                            v
                            @
                            Nearest {
                                square_distance: c, ..
                            },
                        ) if c < square_distance => v,
                        _ => Nearest {
                            square_distance,
                            x: real_x,
                            y: real_y,
                        },
                    });
                }
            }
        }
        if i < threshold.width() {
            let real_x = if flip_x { threshold.width() - 1 - i } else { i };
            for y in 0..std::cmp::min(i, threshold.height()) {
                let real_y = if flip_y {
                    threshold.height() - 1 - y
                } else {
                    y
                };
                if threshold.get_pixel(real_x, real_y) == P::black() {
                    let square_distance = i_squared + y as usize * y as usize;
                    nearest = Some(match nearest {
                        Some(
                            v
                            @
                            Nearest {
                                square_distance: c, ..
                            },
                        ) if c < square_distance => v,
                        _ => Nearest {
                            square_distance,
                            x: real_x,
                            y: real_y,
                        },
                    });
                }
            }
        }
    }

    nearest.map(|n| (n.x, n.y))
}

/// Unperspective and crop an image file.
///
/// # Arguments
///
/// * `input` - The path to the input file.
/// * `output` - The path to the output webp file.
///
/// # Errors
///
/// An error message is returned if the image cannot be loaded, transformed, or saved.
fn crop<PI: AsRef<Path>, PO: AsRef<Path>>(input: PI, output: PO) -> anyhow::Result<()> {
    let img = image::open(input).context("Could not open input")?;
    let luma = img.to_luma8();
    let img = img.into_rgb8();

    let threshold = imageproc::contrast::adaptive_threshold(&luma, 2);
    let closest = [
        find_nearest_to_corner(&threshold, false, false).context("No interesting points")?,
        find_nearest_to_corner(&threshold, true, false).unwrap(),
        find_nearest_to_corner(&threshold, true, true).unwrap(),
        find_nearest_to_corner(&threshold, false, true).unwrap(),
    ];

    let height = std::cmp::max(closest[3].1 - closest[0].1, closest[2].1 - closest[1].1) as f64;
    let width = std::cmp::max(closest[1].0 - closest[0].0, closest[2].0 - closest[3].0) as f64;
    let height_aspect = 9.0 * width / 16.0;
    let width_aspect = 16.0 * height / 9.0;
    let (width, height) = if height_aspect < height {
        (width_aspect, height)
    } else {
        (width, height_aspect)
    };

    const MAX_HEIGHT: f64 = 1024.0;
    const MAX_WIDTH: f64 = 1024.0 * 16.0 / 9.0;
    let height_ratio = MAX_HEIGHT / height;
    let width_ratio = MAX_WIDTH / width;
    let (width, height) = if height_ratio <= width_ratio && height_ratio < 1.0 {
        (width * height_ratio, MAX_HEIGHT)
    } else if width_ratio <= height_ratio && width_ratio < 1.0 {
        (MAX_WIDTH, height * width_ratio)
    } else {
        (width, height)
    };

    let (width, height) = (width.round() as u32, height.round() as u32);

    let projection =
        from_control_points(closest.map(|p| (p.0 as f32, p.1 as f32)), (width, height))?;
    let mut out_img = ImageBuffer::new(width, height);
    imageproc::geometric_transformations::warp_into(
        &img,
        &projection,
        Interpolation::Bicubic,
        Rgb([0, 0, 0]),
        &mut out_img,
    );

    let encoded = Encoder::from_image(&DynamicImage::ImageRgb8(out_img))
        .unwrap()
        .encode(95.0);
    let mut file = File::create(output).context("Could not create output")?;
    file.write_all(&encoded).context("Could not write output")?;
    file.flush().context("Could not write output")?;

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let matches = clap::App::new("qdcrop")
        .author("nil")
        .about("Straighten and remove borders from your Questダンス集会 pictures.")
        .arg(clap::Arg::with_name("input").required(true).multiple(true))
        .arg(
            clap::Arg::with_name("output")
                .short("o")
                .takes_value(true)
                .multiple(true)
                .number_of_values(1),
        )
        .get_matches();

    let mut input = matches.values_of_os("input").unwrap();
    let mut output = matches.values_of_os("output").unwrap_or_default();
    let jobs: Vec<_> = if input.len() > 1 {
        if output.len() > 1 && output.len() != input.len() {
            eprintln!("When multiple inputs and outputs are specified, there must be an equal number of inputs and outputs.");
            process::exit(1);
        }
        if output.len() < 2 {
            let base = output
                .next()
                .map(|o| Path::new(o))
                .unwrap_or_else(|| Path::new("."));
            input
                .map(|i| {
                    let i = Path::new(i);
                    let mut p = base.join(i.file_name().unwrap());
                    p.set_extension("webp");
                    (i, Cow::Owned(p))
                })
                .collect()
        } else {
            input
                .zip(output)
                .map(|(i, o)| (Path::new(i), Cow::Borrowed(Path::new(o))))
                .collect()
        }
    } else {
        if output.len() > 1 {
            eprintln!("When one input is specified, at most one output can be specified.");
            process::exit(1);
        }
        let input = Path::new(input.next().unwrap());
        let output = output
            .next()
            .map(|v| Cow::Borrowed(Path::new(v)))
            .unwrap_or_else(|| {
                let mut p = PathBuf::from(input.file_name().unwrap());
                p.set_extension("webp");
                Cow::Owned(p)
            });
        vec![(input, output)]
    };

    let failed = jobs
        .into_par_iter()
        .map(|(input, output)| {
            if let Err(error) = crop(input, output) {
                eprintln!(
                    "Error while converting {}: {}",
                    input.to_string_lossy(),
                    error
                );
                false
            } else {
                true
            }
        })
        .filter(|success| !success)
        .count();
    if failed > 0 {
        eprintln!("Failed to convert {} inputs", failed);
        process::exit(1);
    }

    Ok(())
}
