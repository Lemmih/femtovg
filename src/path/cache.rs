
use std::ops::Range;
use std::cmp::Ordering;
use std::f32::consts::PI;

use bitflags::bitflags;

use crate::geometry::{self, Bounds, Transform2D};
use crate::renderer::Vertex;
use crate::{Path, Verb, Winding, LineCap, LineJoin, FillRule};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Convexity {
    Concave,
    Convex,
    Unknown
}

impl Default for Convexity {
    fn default() -> Self {
        Self::Unknown
    }
}

bitflags! {
    #[derive(Default)]
    struct PointFlags: u8 {
        const CORNER        = 0x01;
        const LEFT          = 0x02;
        const BEVEL         = 0x04;
        const INNERBEVEL    = 0x08;
    }
}

#[derive(Copy, Clone, Debug, Default)]
struct Point {
    x: f32,
    y: f32,
    dx: f32,
    dy: f32,
    len: f32,
    dmx: f32,
    dmy: f32,
    flags: PointFlags
}

impl Point {
    pub fn new(x: f32, y: f32, flags: PointFlags) -> Self {
        Self {
            x, y, flags, ..Default::default()
        }
    }

    pub fn is_left(p0: &Self, p1: &Self, x: f32, y: f32) -> f32 {
        (p1.x - p0.x) * (y - p0.y) - (x -  p0.x) * (p1.y - p0.y)
    }

    pub fn approx_eq(&self, other: &Self, tolerance: f32) -> bool {
        let dx = other.x - self.x;
        let dy = other.y - self.y;

        dx*dx + dy*dy < tolerance*tolerance
    }
}

#[derive(Clone, Debug)]
pub struct Contour {
    point_range: Range<usize>,
    closed: bool,
    bevel: usize,
    pub(crate) fill: Vec<Vertex>,
    pub(crate) stroke: Vec<Vertex>,
    winding: Winding,
    pub(crate) convexity: Convexity
}

impl Default for Contour {
    fn default() -> Self {
        Self {
            point_range: 0..0,
            closed: Default::default(),
            bevel: Default::default(),
            fill: Default::default(),
            stroke: Default::default(),
            winding: Default::default(),
            convexity: Default::default()
        }
    }
}

impl Contour {
    fn point_pairs<'a>(&self, points: &'a [Point]) -> impl Iterator<Item = (&'a Point, &'a Point)> {
        PointPairsIter {
            curr: 0,
            points: &points[self.point_range.clone()]
        }
    }

    fn polygon_area(points: &[Point]) -> f32 {
        let mut area = 0.0;

        for window in points.windows(3) {
            let p0 = window[0];
            let p1 = window[1];
            let p2 = window[2];

            area += geometry::triarea2(p0.x, p0.y, p1.x, p1.y, p2.x, p2.y);
        }

        area * 0.5
    }

    fn point_count(&self) -> usize {
        self.point_range.end - self.point_range.start
    }
}

struct PointPairsIter<'a> {
    curr: usize,
    points: &'a [Point]
}

impl<'a> Iterator for PointPairsIter<'a> {
    type Item = (&'a Point, &'a Point);

    fn next(&mut self) -> Option<Self::Item> {
        let curr = self.points.get(self.curr);

        let prev = if self.curr == 0 {
            self.points.last()
        } else {
            self.points.get(self.curr - 1)
        };

        self.curr += 1;

        curr.and_then(|some_curr|
            prev.map(|some_prev| (some_prev, some_curr))
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct PathCache {
    pub(crate) contours: Vec<Contour>,
    pub(crate) bounds: Bounds,
    points: Vec<Point>,
}

impl PathCache {

    pub fn new(path: &Path, transform: &Transform2D, tess_tol: f32, dist_tol: f32) -> Self {
        let mut cache = Self::default();

        // Convert path verbs to a set of contours
        for verb in path.verbs() {
            match verb {
                Verb::MoveTo(x, y) => {
                    cache.add_contour();
                    let (x, y) = transform.transform_point(*x, *y);
                    cache.add_point(x, y, PointFlags::CORNER, dist_tol);
                }
                Verb::LineTo(x, y) => {
                    let (x, y) = transform.transform_point(*x, *y);
                    cache.add_point(x, y, PointFlags::CORNER, dist_tol);
                }
                Verb::BezierTo(c1x, c1y, c2x, c2y, x, y) => {
                    if let Some(last) = cache.points.last().copied() {
                        let (c1x, c1y) = transform.transform_point(*c1x, *c1y);
                        let (c2x, c2y) = transform.transform_point(*c2x, *c2y);
                        let (x, y) = transform.transform_point(*x, *y);
                        cache.tesselate_bezier(last.x, last.y, c1x, c1y, c2x, c2y, x, y, 0, PointFlags::CORNER, tess_tol, dist_tol);
                    }
                }
                Verb::Close => if let Some(contour) = cache.contours.last_mut() {
                    contour.closed = true;
                }
                Verb::Winding(winding) => if let Some(contour) = cache.contours.last_mut() {
                    contour.winding = *winding;
                }
            }
        }

        for contour in &mut cache.contours {
            let mut points = &mut cache.points[contour.point_range.clone()];

            // If the first and last points are the same, remove the last, mark as closed contour.
            if let (Some(p0), Some(p1)) = (points.last(), points.first()) {
                if p0.approx_eq(&p1, dist_tol) {
                    contour.point_range.end -= 1;
                    contour.closed = true;
                    points = &mut cache.points[contour.point_range.clone()];
                }
            }

            // Enforce winding.
            let area = Contour::polygon_area(points);

            if contour.winding == Winding::CCW && area < 0.0 {
                points.reverse();
            }

            if contour.winding == Winding::CW && area > 0.0 {
                points.reverse();
            }

            // TODO: this is doggy and fishy.
            for i in 0..contour.point_count() {
                let p1 = points.get(i).copied().unwrap();

                let p0 = if i == 0 {
                    points.last_mut().unwrap()
                } else {
                    points.get_mut(i-1).unwrap()
                };

                p0.dx = p1.x - p0.x;
                p0.dy = p1.y - p0.y;
                p0.len = geometry::normalize(&mut p0.dx, &mut p0.dy);

                cache.bounds.minx = cache.bounds.minx.min(p0.x);
                cache.bounds.miny = cache.bounds.miny.min(p0.y);
                cache.bounds.maxx = cache.bounds.maxx.max(p0.x);
                cache.bounds.maxy = cache.bounds.maxy.max(p0.y);
            }
        }

        // TODO: maybe this can be done in the path instead
        cache.contours.retain(|c| c.point_count() > 1);

        cache
    }

    fn add_contour(&mut self) {
        let mut contour = Contour::default();

        contour.point_range.start = self.points.len();
        contour.point_range.end = self.points.len();

        self.contours.push(contour);
    }

    fn add_point(&mut self, x: f32, y: f32, flags: PointFlags, dist_tol: f32) {
        if let Some(contour) = self.contours.last_mut() {

            let new_point = Point::new(x, y, flags);

            // If last point equals this new point just OR the flags and ignore the new point
            if let Some(last_point) = self.points.get_mut(contour.point_range.end) {
                if last_point.approx_eq(&new_point, dist_tol) {
                    last_point.flags |= new_point.flags;
                    return;
                }
            }

            self.points.push(new_point);
            contour.point_range.end += 1;
        }
    }

    fn tesselate_bezier(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32, x4: f32, y4: f32, level: usize, atype: PointFlags, tess_tol: f32, dist_tol: f32) {
        if level > 10 { return; }

        let x12 = (x1+x2)*0.5;
        let y12 = (y1+y2)*0.5;
        let x23 = (x2+x3)*0.5;
        let y23 = (y2+y3)*0.5;
        let x34 = (x3+x4)*0.5;
        let y34 = (y3+y4)*0.5;
        let x123 = (x12+x23)*0.5;
        let y123 = (y12+y23)*0.5;

        let dx = x4 - x1;
        let dy = y4 - y1;
        let d2 = ((x2 - x4) * dy - (y2 - y4) * dx).abs();
        let d3 = ((x3 - x4) * dy - (y3 - y4) * dx).abs();

        if (d2 + d3)*(d2 + d3) < tess_tol * (dx*dx + dy*dy) {
            self.add_point(x4, y4, atype, dist_tol);
            return;
        }

        let x234 = (x23+x34)*0.5;
        let y234 = (y23+y34)*0.5;
        let x1234 = (x123+x234)*0.5;
        let y1234 = (y123+y234)*0.5;

        self.tesselate_bezier(x1,y1, x12,y12, x123,y123, x1234,y1234, level+1, PointFlags::empty(), tess_tol, dist_tol);
        self.tesselate_bezier(x1234,y1234, x234,y234, x34,y34, x4,y4, level+1, atype, tess_tol, dist_tol);
    }

    pub fn contains_point(&self, x: f32, y: f32, fill_rule: FillRule) -> bool {
        // Early out if point is outside the bounding rectangle
        // TODO: Make this a method on Bounds
        if x < self.bounds.minx || x > self.bounds.maxx || y < self.bounds.miny || y > self.bounds.maxy {
            return false;
        }

        match fill_rule {
            FillRule::EvenOdd => {
                for contour in &self.contours {
                    let mut crossing = false;

                    for (p0, p1) in contour.point_pairs(&self.points) {
                        if (p1.y > y) != (p0.y > y) && (x < (p0.x-p1.x) * (y-p1.y) / (p0.y-p1.y) + p1.x) {
                            crossing = !crossing;
                        }
                    }

                    if crossing {
                        return true;
                    }
                }

                false
            }
            FillRule::NonZero => {
                for contour in &self.contours {
                    let mut winding_number: i32 = 0;

                    for (p0, p1) in contour.point_pairs(&self.points) {
                        if p0.y <= y {
                            if p1.y > y && Point::is_left(p0, p1, x, y) > 0.0 {
                                winding_number = winding_number.wrapping_add(1);
                            }
                        } else if p1.y <= y && Point::is_left(p0, p1, x, y) < 0.0 {
                            winding_number = winding_number.wrapping_sub(1);
                        }
                    }

                    if winding_number != 0 {
                        return true;
                    }
                }

                false
            }
        }
    }

    pub(crate) fn expand_fill(&mut self, stroke_width: f32, line_join: LineJoin, miter_limit: f32, fringe_width: f32) {
        let fringe = stroke_width > 0.0;
        let aa = fringe_width;

        self.calculate_joins(stroke_width, line_join, miter_limit);

        // Calculate max vertex usage.
        for contour in &mut self.contours {
            let point_count = contour.point_count();
            let mut vertex_count = point_count  + contour.bevel + 1;

            if fringe {
                vertex_count += (point_count + contour.bevel*5 + 1) * 2;
            }

            contour.fill.reserve(vertex_count);
        }

        let convex = self.contours.len() == 1 && self.contours[0].convexity == Convexity::Convex;

        for contour in &mut self.contours {
            contour.stroke.clear();

            let woff = 0.5 * aa;

            if fringe {
                for (p0, p1) in contour.point_pairs(&self.points) {
                    if p1.flags.contains(PointFlags::BEVEL) {
                        let dlx0 = p0.dy;
                        let dly0 = -p0.dx;
                        let dlx1 = p1.dy;
                        let dly1 = -p1.dx;

                        if p1.flags.contains(PointFlags::LEFT) {
                            let lx = p1.x + p1.dmx * woff;
                            let ly = p1.y + p1.dmy * woff;
                            contour.fill.push(Vertex::new(lx, ly, 0.5, 1.0));
                        } else {
                            let lx0 = p1.x + dlx0 * woff;
                            let ly0 = p1.y + dly0 * woff;
                            let lx1 = p1.x + dlx1 * woff;
                            let ly1 = p1.y + dly1 * woff;
                            contour.fill.push(Vertex::new(lx0, ly0, 0.5, 1.0));
                            contour.fill.push(Vertex::new(lx1, ly1, 0.5, 1.0));
                        }
                    } else {
                        contour.fill.push(Vertex::new(p1.x + (p1.dmx * woff), p1.y + (p1.dmy * woff), 0.5, 1.0));
                    }
                }
            } else {
                let points = &self.points[contour.point_range.clone()];

                for point in points {
                    contour.fill.push(Vertex::new(point.x, point.y, 0.5, 1.0));
                }
            }

            if fringe {
                let mut lw = stroke_width + woff;
                let rw = stroke_width - woff;
                let mut lu = 0.0;
                let ru = 1.0;

                // Create only half a fringe for convex shapes so that
                // the shape can be rendered without stenciling.
                if convex {
                    lw = woff;    // This should generate the same vertex as fill inset above.
                    lu = 0.5;    // Set outline fade at middle.
                }

                for (p0, p1) in contour.point_pairs(&self.points) {
                    if p1.flags.contains(PointFlags::BEVEL | PointFlags::INNERBEVEL) {
                        bevel_join(&mut contour.stroke, p0, &p1, lw, rw, lu, rw);
                    } else {
                        contour.stroke.push(Vertex::new(p1.x + (p1.dmx * lw), p1.y + (p1.dmy * lw), lu, 1.0));
                        contour.stroke.push(Vertex::new(p1.x - (p1.dmx * rw), p1.y - (p1.dmy * rw), ru, 1.0));
                    }
                }

                // Loop it
                let p0 = contour.stroke[0];
                let p1 = contour.stroke[1];
                contour.stroke.push(Vertex::new(p0.x, p0.y, lu, 1.0));
                contour.stroke.push(Vertex::new(p1.x, p1.y, ru, 1.0));
            }
        }
    }

    // TODO: instead of passing 3 paint values here we can just pass the paint struct as a parameter
    pub(crate) fn expand_stroke(&mut self, stroke_width: f32, fringe: f32, line_cap: LineCap, line_join: LineJoin, miter_limit: f32, tess_tol: f32) {
        let aa = fringe;
        let mut u0 = 0.0;
        let mut u1 = 1.0;
        let ncap = curve_divisions(stroke_width, PI, tess_tol);

        let stroke_width = stroke_width + (aa * 0.5);

        // Disable the gradient used for antialiasing when antialiasing is not used.
        if aa == 0.0 {
            u0 = 0.5;
            u1 = 0.5;
        }

        self.calculate_joins(stroke_width, line_join, miter_limit);

        for contour in &mut self.contours {
            //contour.stroke.clear();

            for (i, (p0, p1)) in contour.point_pairs(&self.points).enumerate() {
                // Add start cap
                if !contour.closed && i == 1 {
                    match line_cap {
                        LineCap::Butt => butt_cap_start(&mut contour.stroke, &p0, &p0, stroke_width, -aa*0.5, aa, u0, u1),
                        LineCap::Square => butt_cap_start(&mut contour.stroke, &p0, &p0, stroke_width, stroke_width-aa, aa, u0, u1),
                        LineCap::Round => round_cap_start(&mut contour.stroke, &p0, &p0, stroke_width, ncap as usize, aa, u0, u1),
                    }
                }

                if (i > 0 && i < contour.point_count() - 1) || contour.closed {
                    if p1.flags.contains(PointFlags::BEVEL) || p1.flags.contains(PointFlags::INNERBEVEL) {
                        if line_join == LineJoin::Round {
                            round_join(&mut contour.stroke, &p0, &p1, stroke_width, stroke_width, u0, u1, ncap as usize);
                        } else {
                            bevel_join(&mut contour.stroke, &p0, &p1, stroke_width, stroke_width, u0, u1);
                        }
                    } else {
                        contour.stroke.push(Vertex::new(p1.x + (p1.dmx * stroke_width), p1.y + (p1.dmy * stroke_width), u0, 1.0));
                        contour.stroke.push(Vertex::new(p1.x - (p1.dmx * stroke_width), p1.y - (p1.dmy * stroke_width), u1, 1.0));
                    }
                }

                // Add end cap
                if !contour.closed && i == contour.point_count() - 1 {
                    match line_cap {
                        LineCap::Butt => butt_cap_end(&mut contour.stroke, &p1, &p0, stroke_width, -aa*0.5, aa, u0, u1),
                        LineCap::Square => butt_cap_end(&mut contour.stroke, &p1, &p0, stroke_width, stroke_width-aa, aa, u0, u1),
                        LineCap::Round => round_cap_end(&mut contour.stroke, &p1, &p0, stroke_width, ncap as usize, aa, u0, u1),
                    }
                }
            }

            if contour.closed {
                contour.stroke.push(Vertex::new(contour.stroke[0].x, contour.stroke[0].y, u0, 1.0));
                contour.stroke.push(Vertex::new(contour.stroke[1].x, contour.stroke[1].y, u1, 1.0));
            }
        }
    }

    fn calculate_joins(&mut self, stroke_width: f32, line_join: LineJoin, miter_limit: f32) {
        let inv_stroke_width = if stroke_width > 0.0 { 1.0 / stroke_width } else { 0.0 };

        for contour in &mut self.contours {
            let points = &mut self.points[contour.point_range.clone()];
            let mut nleft = 0;

            contour.bevel = 0;

            let mut x_sign = 0;
            let mut y_sign = 0;
            let mut x_first_sign = 0; // Sign of first nonzero edge vector x
            let mut y_first_sign = 0; // Sign of first nonzero edge vector y
            let mut x_flips = 0; // Number of sign changes in x
            let mut y_flips = 0; // Number of sign changes in y

            for i in 0..points.len() {

                let p0 = if i == 0 {
                    points.get(points.len()-1).copied().unwrap()
                } else {
                    points.get(i-1).copied().unwrap()
                };

                let p1 = points.get_mut(i).unwrap();

                let dlx0 = p0.dy;
                let dly0 = -p0.dx;
                let dlx1 = p1.dy;
                let dly1 = -p1.dx;

                // Calculate extrusions
                p1.dmx = (dlx0 + dlx1) * 0.5;
                p1.dmy = (dly0 + dly1) * 0.5;
                let dmr2 = p1.dmx * p1.dmx + p1.dmy * p1.dmy;

                if dmr2 > 0.000001 {
                    let scale = (1.0 / dmr2).min(600.0);

                    p1.dmx *= scale;
                    p1.dmy *= scale;
                }

                // Clear flags, but keep the corner.
                p1.flags = if p1.flags.contains(PointFlags::CORNER) { PointFlags::CORNER } else { PointFlags::empty() };

                // Keep track of left turns.
                let cross = p1.dx * p0.dy - p0.dx * p1.dy;

                if cross > 0.0 {
                    nleft += 1;
                    p1.flags |= PointFlags::LEFT;
                }

                // Determine sign for convexity
                match p1.dx.partial_cmp(&0.0) {
                    Some(Ordering::Greater) => {
                        match x_sign.cmp(&0) {
                            Ordering::Equal => x_first_sign = 1,
                            Ordering::Less => x_flips += 1,
                            _ => ()
                        }

                        x_sign = 1;
                    },
                    Some(Ordering::Less) => {
                        match x_sign.cmp(&0) {
                            Ordering::Equal => x_first_sign = -1,
                            Ordering::Greater => x_flips += 1,
                            _ => ()
                        }

                        x_sign = -1;
                    },
                    _ => ()
                }

                match p1.dy.partial_cmp(&0.0) {
                    Some(Ordering::Greater) => {
                        match y_sign.cmp(&0) {
                            Ordering::Equal => y_first_sign = 1,
                            Ordering::Less => y_flips += 1,
                            _ => ()
                        }

                        y_sign = 1;
                    }
                    Some(Ordering::Less) => {
                        match y_sign.cmp(&0) {
                            Ordering::Equal => y_first_sign = -1,
                            Ordering::Greater => y_flips += 1,
                            _ => ()
                        }

                        y_sign = -1;
                    }
                    _ => ()
                }

                // Calculate if we should use bevel or miter for inner join.
                let limit = (p0.len.min(p1.len) * inv_stroke_width).max(1.01);

                if (dmr2 * limit * limit) < 1.0 {
                    p1.flags |= PointFlags::INNERBEVEL;
                }

                // Check to see if the corner needs to be beveled.
                if p1.flags.contains(PointFlags::CORNER) {
                    if (dmr2 * miter_limit * miter_limit) < 1.0 || line_join == LineJoin::Bevel || line_join == LineJoin::Round {
                        p1.flags |= PointFlags::BEVEL;
                    }
                }

                if p1.flags.contains(PointFlags::BEVEL | PointFlags::INNERBEVEL) {
                    contour.bevel += 1;
                }
            }

            if x_sign != 0 && x_first_sign != 0 && x_sign != x_first_sign {
                x_flips += 1;
            }

            if y_sign != 0 && y_first_sign != 0 && y_sign != y_first_sign {
                y_flips += 1;
            }

            let convex = x_flips == 2 && y_flips == 2;

            contour.convexity = if nleft == points.len() && convex { Convexity::Convex } else { Convexity::Concave };
        }
    }
}

fn curve_divisions(radius: f32, arc: f32, tol: f32) -> u32 {
    let da = (radius / (radius + tol)).acos() * 2.0;

    ((arc / da).ceil() as u32).max(2)
}

fn butt_cap_start(verts: &mut Vec<Vertex>, p0: &Point, p1: &Point, w: f32, d: f32, aa: f32, u0: f32, u1: f32) {
    let px = p0.x - p1.dx*d;
    let py = p0.y - p1.dy*d;
    let dlx = p1.dy;
    let dly = -p1.dx;

    verts.push(Vertex::new(px + dlx*w - p1.dx*aa, py + dly*w - p1.dy*aa, u0, 0.0));
    verts.push(Vertex::new(px - dlx*w - p1.dx*aa, py - dly*w - p1.dy*aa, u1, 0.0));
    verts.push(Vertex::new(px + dlx*w, py + dly*w, u0, 1.0));
    verts.push(Vertex::new(px - dlx*w, py - dly*w, u1, 1.0));
}

fn butt_cap_end(verts: &mut Vec<Vertex>, p0: &Point, p1: &Point, w: f32, d: f32, aa: f32, u0: f32, u1: f32) {
    let px = p0.x + p1.dx*d;
    let py = p0.y + p1.dy*d;
    let dlx = p1.dy;
    let dly = -p1.dx;

    verts.push(Vertex::new(px + dlx*w, py + dly*w, u0, 1.0));
    verts.push(Vertex::new(px - dlx*w, py - dly*w, u1, 1.0));
    verts.push(Vertex::new(px + dlx*w + p1.dx*aa, py + dly*w + p1.dy*aa, u0, 0.0));
    verts.push(Vertex::new(px - dlx*w + p1.dx*aa, py - dly*w + p1.dy*aa, u1, 0.0));
}

fn round_cap_start(verts: &mut Vec<Vertex>, p0: &Point, p1: &Point, w: f32, ncap: usize, _aa: f32, u0: f32, u1: f32) {
    let px = p0.x;
    let py = p0.y;
    let dlx = p1.dy;
    let dly = -p1.dx;

    for i in 0..ncap {
        let a = i as f32/(ncap as f32 - 1.0)*PI;
        let ax = a.cos() * w;
        let ay = a.sin() * w;

        verts.push(Vertex::new(px - dlx*ax - p1.dx*ay, py - dly*ax - p1.dy*ay, u0, 1.0));
        verts.push(Vertex::new(px, py, 0.5, 1.0));
    }

    verts.push(Vertex::new(px + dlx*w, py + dly*w, u0, 1.0));
    verts.push(Vertex::new(px - dlx*w, py - dly*w, u1, 1.0));
}

fn round_cap_end(verts: &mut Vec<Vertex>, p0: &Point, p1: &Point, w: f32, ncap: usize, _aa: f32, u0: f32, u1: f32) {
    let px = p0.x;
    let py = p0.y;
    let dlx = p1.dy;
    let dly = -p1.dx;

    verts.push(Vertex::new(px + dlx*w, py + dly*w, u0, 1.0));
    verts.push(Vertex::new(px - dlx*w, py - dly*w, u1, 1.0));

    for i in 0..ncap {
        let a = i as f32/(ncap as f32 - 1.0)*PI;
        let ax = a.cos() * w;
        let ay = a.sin() * w;

        verts.push(Vertex::new(px, py, 0.5, 1.0));
        verts.push(Vertex::new(px - dlx*ax + p1.dx*ay, py - dly*ax + p1.dy*ay, u0, 1.0));
    }
}

fn choose_bevel(bevel: bool, p0: &Point, p1: &Point, w: f32) -> (f32, f32, f32, f32) {
    if bevel {
        (p1.x + p0.dy * w, p1.y - p0.dx * w, p1.x + p1.dy * w, p1.y - p1.dx * w)
    } else {
        (p1.x + p1.dmx * w, p1.y + p1.dmy * w, p1.x + p1.dmx * w, p1.y + p1.dmy * w)
    }
}

fn round_join(verts: &mut Vec<Vertex>, p0: &Point, p1: &Point, lw: f32, rw: f32, lu: f32, ru: f32, ncap: usize) {
    let dlx0 = p0.dy;
    let dly0 = -p0.dx;
    let dlx1 = p1.dy;
    let dly1 = -p1.dx;

    let a0;
    let mut a1;

    if p1.flags.contains(PointFlags::LEFT) {
        let (lx0, ly0, lx1, ly1) = choose_bevel(p1.flags.contains(PointFlags::INNERBEVEL), p0, p1, lw);
        a0 = (-dly0).atan2(-dlx0);
        a1 = (-dly1).atan2(-dlx1);

        if a1 > a0 {
            a1 -= PI * 2.0;
        }

        verts.push(Vertex::new(lx0, ly0, lu, 1.0));
        verts.push(Vertex::new(p1.x - dlx0*rw, p1.y - dly0*rw, ru, 1.0));

        let n = ((((a0 - a1) / PI) * ncap as f32).ceil() as usize).max(2).min(ncap);

        for i in 0..n {
            let u = i as f32 / (n-1) as f32;
            let a = a0 + u*(a1-a0);
            let rx = p1.x + a.cos() * rw;
            let ry = p1.y + a.sin() * rw;

            verts.push(Vertex::new(p1.x, p1.y, 0.5, 1.0));
            verts.push(Vertex::new(rx, ry, ru, 1.0));
        }

        verts.push(Vertex::new(lx1, ly1, lu, 1.0));
        verts.push(Vertex::new(p1.x - dlx1*rw, p1.y - dly1*rw, ru, 1.0));
    } else {
        let (rx0, ry0, rx1, ry1) = choose_bevel(p1.flags.contains(PointFlags::INNERBEVEL), p0, p1, -rw);
        a0 = dly0.atan2(dlx0);
        a1 = dly1.atan2(dlx1);

        if a1 < a0 {
            a1 += PI * 2.0;
        }

        verts.push(Vertex::new(p1.x + dlx0*rw, p1.y + dly0*rw, lu, 1.0));
        verts.push(Vertex::new(rx0, ry0, ru, 1.0));

        let n = ((((a1 - a0) / PI) * ncap as f32).ceil() as usize).max(2).min(ncap);

        for i in 0..n {
            let u = i as f32 / (n-1) as f32;
            let a = a0 + u*(a1-a0);
            let lx = p1.x + a.cos() * lw;
            let ly = p1.y + a.sin() * lw;

            verts.push(Vertex::new(lx, ly, lu, 1.0));
            verts.push(Vertex::new(p1.x, p1.y, 0.5, 1.0));
        }

        verts.push(Vertex::new(p1.x + dlx1*rw, p1.y + dly1*rw, lu, 1.0));
        verts.push(Vertex::new(rx1, ry1, ru, 1.0));
    }
}

fn bevel_join(verts: &mut Vec<Vertex>, p0: &Point, p1: &Point, lw: f32, rw: f32, lu: f32, ru: f32) {
    let dlx0 = p0.dy;
    let dly0 = -p0.dx;
    let dlx1 = p1.dy;
    let dly1 = -p1.dx;

    if p1.flags.contains(PointFlags::LEFT) {
        let (lx0, ly0, lx1, ly1) = choose_bevel(p1.flags.contains(PointFlags::INNERBEVEL), p0, p1, lw);

        verts.push(Vertex::new(lx0, ly0, lu, 1.0));
        verts.push(Vertex::new(p1.x - dlx0*rw, p1.y - dly0*rw, ru, 1.0));

        if p1.flags.contains(PointFlags::BEVEL) {
            verts.push(Vertex::new(lx0, ly0, lu, 1.0));
            verts.push(Vertex::new(p1.x - dlx0*rw, p1.y - dly0*rw, ru, 1.0));

            verts.push(Vertex::new(lx1, ly1, lu, 1.0));
            verts.push(Vertex::new(p1.x - dlx1*rw, p1.y - dly1*rw, ru, 1.0));
        } else {
            let rx0 = p1.x - p1.dmx * rw;
            let ry0 = p1.y - p1.dmy * rw;

            verts.push(Vertex::new(p1.x, p1.y, 0.5, 1.0));
            verts.push(Vertex::new(p1.x - dlx0*rw, p1.y - dly0*rw, ru, 1.0));

            verts.push(Vertex::new(rx0, ry0, ru, 1.0));
            verts.push(Vertex::new(rx0, ry0, ru, 1.0));

            verts.push(Vertex::new(p1.x, p1.y, 0.5, 1.0));
            verts.push(Vertex::new(p1.x - dlx1*rw, p1.y - dly1*rw, ru, 1.0));
        }

        verts.push(Vertex::new(lx1, ly1, lu, 1.0));
        verts.push(Vertex::new(p1.x - dlx1*rw, p1.y - dly1*rw, ru, 1.0));
    } else {
        let (rx0, ry0, rx1, ry1) = choose_bevel(p1.flags.contains(PointFlags::INNERBEVEL), p0, p1, -rw);

        verts.push(Vertex::new(p1.x + dlx0*lw, p1.y + dly0*lw, lu, 1.0));
        verts.push(Vertex::new(rx0, ry0, ru, 1.0));

        if p1.flags.contains(PointFlags::BEVEL) {
            verts.push(Vertex::new(p1.x + dlx0*lw, p1.y + dly0*lw, lu, 1.0));
            verts.push(Vertex::new(rx0, ry0, ru, 1.0));

            verts.push(Vertex::new(p1.x + dlx1*lw, p1.y + dly1*lw, lu, 1.0));
            verts.push(Vertex::new(rx1, ry1, ru, 1.0));
        } else {
            let lx0 = p1.x + p1.dmx * lw;
            let ly0 = p1.y + p1.dmy * lw;

            verts.push(Vertex::new(p1.x + dlx0*lw, p1.y + dly0*lw, lu, 1.0));
            verts.push(Vertex::new(p1.x, p1.y, 0.5, 1.0));

            verts.push(Vertex::new(lx0, ly0, lu, 1.0));
            verts.push(Vertex::new(lx0, ly0, lu, 1.0));

            verts.push(Vertex::new(p1.x + dlx1*lw, p1.y + dly1*lw, lu, 1.0));
            verts.push(Vertex::new(p1.x, p1.y, 0.5, 1.0));
        }

        verts.push(Vertex::new(p1.x + dlx1*lw, p1.y + dly1*lw, lu, 1.0));
        verts.push(Vertex::new(rx1, ry1, ru, 1.0));
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn self_intersecting_polygon_is_concave() {
        // star
        let mut path = Path::new();
        path.move_to(50.0, 0.0);
        path.line_to(21.0, 90.0);
        path.line_to(98.0, 35.0);
        path.line_to(2.0, 35.0);
        path.line_to(79.0, 90.0);
        path.close();

        let transform = Transform2D::identity();

        let mut path_cache = PathCache::new(&path, &transform, 0.25, 0.01);
        path_cache.expand_fill(1.0, LineJoin::Miter, 10.0, 1.0);

        assert_eq!(path_cache.contours[0].convexity, Convexity::Concave);
    }
}