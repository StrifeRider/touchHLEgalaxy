/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `CATransform3D.h`
//!
//! Like `CGAffineTransform`, this uses row-vector (v × M) conventions, so
//! `CATransform3DConcat(a, b)` means "apply a, then b".
//!
//! touchHLE's compositor is 2D-only, so when a 3D transform is applied to a
//! layer, it is projected to the equivalent affine transform (ignoring Z and
//! perspective), see [CATransform3D::to_affine].

use crate::abi::{impl_GuestRet_for_large_struct, GuestArg};
use crate::dyld::{export_c_func, FunctionExports};
use crate::frameworks::core_graphics::cg_affine_transform::CGAffineTransform;
use crate::frameworks::core_graphics::CGFloat;
use crate::mem::SafeRead;
use crate::Environment;

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C, packed)]
pub struct CATransform3D {
    /// Row-major: `m[0]` is m11, `m[1]` is m12, ... `m[15]` is m44.
    pub m: [CGFloat; 16],
}
unsafe impl SafeRead for CATransform3D {}
impl GuestArg for CATransform3D {
    const REG_COUNT: usize = 16;

    fn from_regs(regs: &[u32]) -> Self {
        let mut m = [0.0; 16];
        for (i, v) in m.iter_mut().enumerate() {
            *v = GuestArg::from_regs(&regs[i..i + 1]);
        }
        CATransform3D { m }
    }
    fn to_regs(self, regs: &mut [u32]) {
        let m = self.m;
        for (i, v) in m.iter().enumerate() {
            v.to_regs(&mut regs[i..i + 1]);
        }
    }
}
impl_GuestRet_for_large_struct!(CATransform3D);

impl CATransform3D {
    pub const IDENTITY: CATransform3D = CATransform3D {
        m: [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
    };

    fn get(&self, row: usize, col: usize) -> CGFloat {
        let m = self.m;
        m[row * 4 + col]
    }

    /// `self × other` (apply self, then other).
    pub fn concat(&self, other: &CATransform3D) -> CATransform3D {
        let mut m = [0.0; 16];
        for r in 0..4 {
            for c in 0..4 {
                m[r * 4 + c] = (0..4).map(|k| self.get(r, k) * other.get(k, c)).sum();
            }
        }
        CATransform3D { m }
    }

    pub fn make_rotation(angle: CGFloat, x: CGFloat, y: CGFloat, z: CGFloat) -> CATransform3D {
        let len = (x * x + y * y + z * z).sqrt();
        if len == 0.0 {
            return Self::IDENTITY;
        }
        let (x, y, z) = (x / len, y / len, z / len);
        let (s, c) = angle.sin_cos();
        let t = 1.0 - c;
        CATransform3D {
            m: [
                t * x * x + c,
                t * x * y + s * z,
                t * x * z - s * y,
                0.0,
                t * x * y - s * z,
                t * y * y + c,
                t * y * z + s * x,
                0.0,
                t * x * z + s * y,
                t * y * z - s * x,
                t * z * z + c,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
        }
    }

    pub fn make_scale(sx: CGFloat, sy: CGFloat, sz: CGFloat) -> CATransform3D {
        let mut t = Self::IDENTITY;
        t.m[0] = sx;
        t.m[5] = sy;
        t.m[10] = sz;
        t
    }

    pub fn make_translation(tx: CGFloat, ty: CGFloat, tz: CGFloat) -> CATransform3D {
        let mut t = Self::IDENTITY;
        t.m[12] = tx;
        t.m[13] = ty;
        t.m[14] = tz;
        t
    }

    pub fn from_affine(a: CGAffineTransform) -> CATransform3D {
        let mut t = Self::IDENTITY;
        t.m[0] = a.a;
        t.m[1] = a.b;
        t.m[4] = a.c;
        t.m[5] = a.d;
        t.m[12] = a.tx;
        t.m[13] = a.ty;
        t
    }

    pub fn to_affine(&self) -> CGAffineTransform {
        CGAffineTransform {
            a: self.get(0, 0),
            b: self.get(0, 1),
            c: self.get(1, 0),
            d: self.get(1, 1),
            tx: self.get(3, 0),
            ty: self.get(3, 1),
        }
    }
}

fn CATransform3DIsIdentity(_env: &mut Environment, t: CATransform3D) -> bool {
    t == CATransform3D::IDENTITY
}
fn CATransform3DEqualToTransform(
    _env: &mut Environment,
    a: CATransform3D,
    b: CATransform3D,
) -> bool {
    a == b
}
fn CATransform3DMakeTranslation(
    _env: &mut Environment,
    tx: CGFloat,
    ty: CGFloat,
    tz: CGFloat,
) -> CATransform3D {
    CATransform3D::make_translation(tx, ty, tz)
}
fn CATransform3DMakeScale(
    _env: &mut Environment,
    sx: CGFloat,
    sy: CGFloat,
    sz: CGFloat,
) -> CATransform3D {
    CATransform3D::make_scale(sx, sy, sz)
}
fn CATransform3DMakeRotation(
    _env: &mut Environment,
    angle: CGFloat,
    x: CGFloat,
    y: CGFloat,
    z: CGFloat,
) -> CATransform3D {
    CATransform3D::make_rotation(angle, x, y, z)
}
fn CATransform3DTranslate(
    _env: &mut Environment,
    t: CATransform3D,
    tx: CGFloat,
    ty: CGFloat,
    tz: CGFloat,
) -> CATransform3D {
    CATransform3D::make_translation(tx, ty, tz).concat(&t)
}
fn CATransform3DScale(
    _env: &mut Environment,
    t: CATransform3D,
    sx: CGFloat,
    sy: CGFloat,
    sz: CGFloat,
) -> CATransform3D {
    CATransform3D::make_scale(sx, sy, sz).concat(&t)
}
fn CATransform3DRotate(
    _env: &mut Environment,
    t: CATransform3D,
    angle: CGFloat,
    x: CGFloat,
    y: CGFloat,
    z: CGFloat,
) -> CATransform3D {
    CATransform3D::make_rotation(angle, x, y, z).concat(&t)
}
fn CATransform3DConcat(
    _env: &mut Environment,
    a: CATransform3D,
    b: CATransform3D,
) -> CATransform3D {
    a.concat(&b)
}
fn CATransform3DMakeAffineTransform(_env: &mut Environment, a: CGAffineTransform) -> CATransform3D {
    CATransform3D::from_affine(a)
}
fn CATransform3DIsAffine(_env: &mut Environment, t: CATransform3D) -> bool {
    CATransform3D::from_affine(t.to_affine()) == t
}
fn CATransform3DGetAffineTransform(_env: &mut Environment, t: CATransform3D) -> CGAffineTransform {
    t.to_affine()
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(CATransform3DIsIdentity(_)),
    export_c_func!(CATransform3DEqualToTransform(_, _)),
    export_c_func!(CATransform3DMakeTranslation(_, _, _)),
    export_c_func!(CATransform3DMakeScale(_, _, _)),
    export_c_func!(CATransform3DMakeRotation(_, _, _, _)),
    export_c_func!(CATransform3DTranslate(_, _, _, _)),
    export_c_func!(CATransform3DScale(_, _, _, _)),
    export_c_func!(CATransform3DRotate(_, _, _, _, _)),
    export_c_func!(CATransform3DConcat(_, _)),
    export_c_func!(CATransform3DMakeAffineTransform(_)),
    export_c_func!(CATransform3DIsAffine(_)),
    export_c_func!(CATransform3DGetAffineTransform(_)),
];
