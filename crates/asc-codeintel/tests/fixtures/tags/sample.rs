use std::fmt;

/// A point in the plane.
/// Coordinates are integers.
#[derive(Debug)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

pub enum Shape {
    Dot(Point),
    Line,
}

pub trait Area {
    fn area(&self) -> f64;
}

impl Area for Shape {
    fn area(&self) -> f64 {
        helper(1.0)
    }
}

impl Point {
    /// Make a point.
    pub fn new(x: i32, y: i32) -> Self {
        let local = 1;
        Point { x, y }
    }
}

const LIMIT: usize = 4;

pub mod inner {
    pub fn deep() {
        println!("hi");
    }
}

fn helper(v: f64) -> f64 {
    const SCRATCH: u8 = 1;
    Point::new(1, 2);
    v
}
