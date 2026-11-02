/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Functions, traits, and all kinds of things to assist with bridging the gap
//! between guest and host when it comes to animations in Core Animation.
//! Based in Apple's documented behavior for Core Animation, although not an
//! exact match.
//! References:
//! - Core Animation Programming Guide
//!   <https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/CoreAnimation_guide/Introduction/Introduction.html>
//! - List of Animatable properties
//!   <https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/CoreAnimation_guide/AnimatableProperties/AnimatableProperties.html#//apple_ref/doc/uid/TP40004514-CH11-SW2>
//! - Animation timing behavior, layers' local time, autoreverses, etc.
//!   <https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/CoreAnimation_guide/AdvancedAnimationTricks/AdvancedAnimationTricks.html>
//! - Algorithm for choosing interpolation values
//!   <https://developer.apple.com/documentation/quartzcore/cabasicanimation?language=objc>
use std::ops::Sub;

use crate::frameworks::core_animation::ca_animation::keyframe_segment;
use crate::frameworks::core_animation::ca_animation::{
    get_animation_start_time, kCAFillModeBackwards, kCAFillModeBoth, kCAFillModeForwards,
    CAMediaTimingFillMode,
};
use crate::frameworks::core_animation::ca_layer::remove_anonymous_animation;
use crate::frameworks::core_animation::ca_transform3d::CATransform3D;
use crate::frameworks::core_animation::{ca_layer::CALayerHostObject, CACurrentMediaTime};
use crate::frameworks::core_foundation::time::CFTimeInterval;
use crate::frameworks::core_graphics::cg_affine_transform::CGAffineTransform;
use crate::frameworks::core_graphics::cg_color::CGColorHostObject;
use crate::frameworks::foundation::ns_string::{from_rust_string, to_rust_string};
use crate::objc::{id, msg, nil, release, retain};
use crate::Environment;

#[derive(Default)]
pub struct State {
    started_animations: Vec<id>,
    finished_animations: Vec<(id, id, bool, bool, Option<String>)>,
}
impl State {
    pub fn create_presentation_layer(
        &mut self,
        env: &mut Environment,
        layer: id,
    ) -> CALayerHostObject {
        // Clone given layer
        let original = env.objc.borrow::<CALayerHostObject>(layer);
        let mut presentation = original.clone();

        // Loop over all animations and set the presentation layer's values
        let named_animations: Vec<(Option<String>, id)> = presentation
            .animations
            .iter()
            .map(|(key, anim)| (Some(key.clone()), *anim))
            .collect();
        let anonymous_animations: Vec<(Option<String>, id)> = presentation
            .anonymous_animations
            .iter()
            .map(|anim| (None, *anim))
            .collect();

        for (key, animation) in
            Iterator::chain(named_animations.iter(), anonymous_animations.iter())
        {
            let animation = *animation;

            let fill_mode: CAMediaTimingFillMode = msg![env; animation fillMode];
            let fill_mode = to_rust_string(env, fill_mode);

            // TODO: Convert to local time
            let current_time = CACurrentMediaTime(env);
            let begin_time: CFTimeInterval = msg![env; animation beginTime];
            let start_time = get_animation_start_time(env, animation);

            if current_time >= begin_time {
                if start_time.is_none() {
                    // Animation started but isn't marked as such
                    start_time.replace(current_time);
                    self.started_animations.push(animation);
                }
            } else if fill_mode != kCAFillModeBackwards && fill_mode != kCAFillModeBoth {
                continue;
            }

            let effective_begin_time = start_time.unwrap_or(begin_time);

            if let Some(key) = key {
                log_dbg!(
                    "Animate CALayer {:?} animation {} {:?}",
                    layer,
                    key,
                    animation
                );
            } else {
                log_dbg!("Animate CALayer {:?} animation {:?}", layer, animation);
            }

            let repeat_count: f32 = msg![env; animation repeatCount];
            assert!(repeat_count >= 0.0);
            // Setting [repeatCount] to greatestFiniteMagnitude will cause
            // the animation to repeat forever.
            let effective_repeat_count = if repeat_count == f32::MAX {
                f32::INFINITY
            } else if repeat_count == 0.0 {
                1.0
            } else {
                repeat_count
            };

            let duration: CFTimeInterval = msg![env; animation duration];
            let time_offset: CFTimeInterval = msg![env; animation timeOffset];
            let current_repeat = (((current_time - effective_begin_time + time_offset).max(0.0)
                / duration) as f32)
                .min(effective_repeat_count);

            let mut progress = current_repeat.fract();

            let autoreverses: bool = msg![env; animation autoreverses];
            if autoreverses {
                // From the docs:
                // Setting the repeat count to a whole number (such as 1.0) for
                // an autoreversing animation causes the animation to stop on
                // its starting value.
                // Adding an extra half step (such as a repeat count of 1.5)
                // causes the animation to stop on its end value
                progress = ((progress * 2.0 - 1.0).abs() - 1.0).abs();
            }

            let timing_function: id = msg![env; animation timingFunction];
            let interpolation_amount: f32 = msg![env; timing_function _solveForInput: progress];
            let cumulative: bool = msg![env; animation isCumulative];

            if current_repeat >= effective_repeat_count {
                let removed_on_completion: bool = msg![env; animation isRemovedOnCompletion];
                self.finished_animations.push((
                    layer,
                    animation,
                    true,
                    removed_on_completion,
                    key.to_owned(),
                ));
                if fill_mode != kCAFillModeForwards && fill_mode != kCAFillModeBoth {
                    continue;
                }
            }

            // CABasicAnimation, or CAKeyframeAnimation (which is reduced
            // to a basic animation between the two surrounding keyframes).
            // TODO: Handle other types of animations
            let (from_value, to_value, by_value, interpolation_amount) =
                if let Some((from, to, amount)) =
                    keyframe_segment(env, animation, interpolation_amount)
                {
                    (from, to, nil, amount)
                } else {
                    let from_value: id = msg![env; animation fromValue];
                    let to_value: id = msg![env; animation toValue];
                    let by_value: id = msg![env; animation byValue];
                    (from_value, to_value, by_value, interpolation_amount)
                };
            // Cumulative animations build on the end value of each previous
            // repetition, which for linear interpolation is extrapolation.
            let interpolation_amount = if cumulative && !is_keyframe(env, animation) {
                interpolation_amount
                    + current_repeat
                        .min(effective_repeat_count - 1.0)
                        .max(0.0)
                        .floor()
            } else {
                interpolation_amount
            };

            // Update values only in the presentation layer
            // If the animation has no key path, Core Animation uses the key
            // it was added to the layer with.
            let key_path: id = msg![env; animation keyPath];
            let key_path = if key_path != nil {
                to_rust_string(env, key_path).to_string()
            } else if let Some(key) = key {
                key.clone()
            } else {
                log!(
                    "TODO: Animation {:?} has no key path or key, ignoring",
                    animation
                );
                continue;
            };
            // Only these properties are animatable
            // TODO: Implement for all properties
            match &*key_path {
                "anchorPoint" => {
                    let from_value =
                        id_as_option(from_value).map(|obj| msg![env; obj CGPointValue]);
                    let to_value = id_as_option(to_value).map(|obj| msg![env; obj CGPointValue]);
                    let by_value = id_as_option(by_value).map(|obj| msg![env; obj CGPointValue]);
                    let (from_value, by_value) = get_from_and_by_values(
                        Some(presentation.anchor_point),
                        from_value,
                        to_value,
                        by_value,
                    );
                    presentation.anchor_point = from_value + by_value * interpolation_amount;
                }
                "backgroundColor" => {
                    let from_value = id_as_option(from_value)
                        .map(|obj| *env.objc.borrow::<CGColorHostObject>(obj));
                    let to_value = id_as_option(to_value)
                        .map(|obj| *env.objc.borrow::<CGColorHostObject>(obj));
                    let by_value = id_as_option(by_value)
                        .map(|obj| *env.objc.borrow::<CGColorHostObject>(obj));
                    let (from_value, by_value) = get_from_and_by_values(
                        presentation.background_color,
                        from_value,
                        to_value,
                        by_value,
                    );
                    presentation.background_color =
                        Some(from_value + by_value * interpolation_amount)
                }
                "bounds" => {
                    let from_value = id_as_option(from_value).map(|obj| msg![env; obj CGRectValue]);
                    let to_value = id_as_option(to_value).map(|obj| msg![env; obj CGRectValue]);
                    let by_value = id_as_option(by_value).map(|obj| msg![env; obj CGRectValue]);
                    let (from_value, by_value) = get_from_and_by_values(
                        Some(presentation.bounds),
                        from_value,
                        to_value,
                        by_value,
                    );
                    presentation.bounds = from_value + by_value * interpolation_amount;
                }
                "cornerRadius" => {
                    let from_value = id_as_option(from_value).map(|obj| msg![env; obj floatValue]);
                    let to_value = id_as_option(to_value).map(|obj| msg![env; obj floatValue]);
                    let by_value = id_as_option(by_value).map(|obj| msg![env; obj floatValue]);
                    let (from_value, by_value) = get_from_and_by_values(
                        Some(presentation.corner_radius),
                        from_value,
                        to_value,
                        by_value,
                    );
                    presentation.corner_radius = from_value + by_value * interpolation_amount;
                }
                "hidden" => {
                    let from_value = id_as_option(from_value)
                        .map(|obj| msg![env; obj boolValue])
                        .map(|val: bool| val as i32 as f32);
                    let to_value = id_as_option(to_value)
                        .map(|obj| msg![env; obj boolValue])
                        .map(|val: bool| val as i32 as f32);
                    let by_value = id_as_option(by_value)
                        .map(|obj| msg![env; obj boolValue])
                        .map(|val: bool| val as i32 as f32);
                    let (from_value, by_value) = get_from_and_by_values(
                        Some(presentation.hidden as i32 as f32),
                        from_value,
                        to_value,
                        by_value,
                    );
                    presentation.hidden = (from_value + by_value * interpolation_amount) > 0.5;
                }
                "opacity" => {
                    let from_value = id_as_option(from_value).map(|obj| msg![env; obj floatValue]);
                    let to_value = id_as_option(to_value).map(|obj| msg![env; obj floatValue]);
                    let by_value = id_as_option(by_value).map(|obj| msg![env; obj floatValue]);
                    let (from_value, by_value) = get_from_and_by_values(
                        Some(presentation.opacity),
                        from_value,
                        to_value,
                        by_value,
                    );
                    presentation.opacity = from_value + by_value * interpolation_amount;
                }
                "position" => {
                    let from_value =
                        id_as_option(from_value).map(|obj| msg![env; obj CGPointValue]);
                    let to_value = id_as_option(to_value).map(|obj| msg![env; obj CGPointValue]);
                    let by_value = id_as_option(by_value).map(|obj| msg![env; obj CGPointValue]);
                    let (from_value, by_value) = get_from_and_by_values(
                        Some(presentation.position),
                        from_value,
                        to_value,
                        by_value,
                    );
                    presentation.position = from_value + by_value * interpolation_amount;
                }
                "transform" => {
                    let current = CATransform3D::from_affine(presentation.affine_transform);
                    let get = |env: &mut Environment, v: id| -> Option<CATransform3D> {
                        id_as_option(v).map(|obj| msg![env; obj CATransform3DValue])
                    };
                    let from_t = get(env, from_value);
                    let to_t = get(env, to_value);
                    let by_t = get(env, by_value);
                    let (from_t, to_t) = match (from_t, to_t, by_t) {
                        (Some(f), Some(t), _) => (f, t),
                        (Some(f), None, Some(b)) => (f, f.concat(&b)),
                        (None, Some(t), _) => (current, t),
                        (Some(f), None, None) => (f, current),
                        (None, None, Some(b)) => (current, current.concat(&b)),
                        (None, None, None) => (current, current),
                    };
                    presentation.affine_transform = interpolate_affine(
                        from_t.to_affine(),
                        to_t.to_affine(),
                        interpolation_amount,
                    );
                }
                _ => {
                    log!("TODO: Attempted to animate on key {}, ignoring", key_path);
                }
            }
        }

        presentation
    }

    pub fn update_started_and_finished_animations(self, env: &mut Environment) {
        // Delegate methods can run arbitrary app code, which may remove (and
        // thus free) other animations or layers in these lists, so keep them
        // all alive until we're done.
        for &animation in &self.started_animations {
            retain(env, animation);
        }
        for &(layer, animation, ..) in &self.finished_animations {
            retain(env, layer);
            retain(env, animation);
        }

        // The delegate methods are optional.
        let did_start_sel = env
            .objc
            .register_host_selector("animationDidStart:".to_string(), &mut env.mem);
        let did_stop_sel = env
            .objc
            .register_host_selector("animationDidStop:finished:".to_string(), &mut env.mem);

        for &animation in &self.started_animations {
            let delegate = msg![env; animation delegate];
            if delegate != nil && msg![env; delegate respondsToSelector:did_start_sel] {
                () = msg![env; delegate animationDidStart: animation];
            }
        }

        for (layer, animation, finished, removed_on_completion, key) in &self.finished_animations {
            let (layer, animation, finished) = (*layer, *animation, *finished);
            // An earlier delegate call may have removed this animation
            // already, in which case there's nothing more to do.
            if !is_animation_attached(env, layer, animation, key.as_deref()) {
                continue;
            }
            // Like Core Animation, remove the animation before telling the
            // delegate, so that if the delegate adds a new animation for the
            // same key, it isn't removed.
            if *removed_on_completion {
                if let Some(key) = key {
                    let key = from_rust_string(env, key.clone());
                    () = msg![env; layer removeAnimationForKey: key];
                } else {
                    remove_anonymous_animation(env, layer, animation);
                }
            }

            let delegate = msg![env; animation delegate];
            if delegate != nil && msg![env; delegate respondsToSelector:did_stop_sel] {
                () = msg![env; delegate animationDidStop: animation finished: finished];
            }
        }

        for animation in self.started_animations {
            release(env, animation);
        }
        for (layer, animation, ..) in self.finished_animations {
            release(env, animation);
            release(env, layer);
        }
    }
}

/// Whether `animation` is (still) attached to `layer`, under `key` if given.
fn is_animation_attached(env: &Environment, layer: id, animation: id, key: Option<&str>) -> bool {
    let host_obj = env.objc.borrow::<CALayerHostObject>(layer);
    match key {
        Some(key) => host_obj.animations.get(key) == Some(&animation),
        None => host_obj.anonymous_animations.contains(&animation),
    }
}

fn get_from_and_by_values<T>(
    current_value: Option<T>,
    from_value: Option<T>,
    to_value: Option<T>,
    by_value: Option<T>,
) -> (T, T)
where
    T: Copy + Sub<Output = T>,
{
    if from_value.is_some() && to_value.is_some() && by_value.is_some() {
        panic!("Cannot specify all three of fromValue, toValue, and byValue");
    } else if let (Some(from_value), Some(to_value)) = (from_value, to_value) {
        let by_value = to_value - from_value.to_owned();
        (from_value, by_value)
    } else if let (Some(from_value), Some(by_value)) = (from_value, by_value) {
        (from_value.to_owned(), by_value.to_owned())
    } else if let (Some(to_value), Some(by_value)) = (to_value, by_value) {
        let from_value = to_value - by_value;
        (from_value, by_value.to_owned())
    } else if let Some(from_value) = from_value {
        let by_value = current_value.unwrap() - from_value;
        (from_value.to_owned(), by_value)
    } else if let Some(to_value) = to_value {
        let from_value = current_value.unwrap();
        let by_value = to_value - from_value;
        (from_value.to_owned(), by_value)
    } else if let Some(by_value) = by_value {
        let from_value = current_value.unwrap();
        (from_value.to_owned(), by_value.to_owned())
    } else {
        // TODO: All properties are nil. Interpolates between the previous
        // value of keyPath in the target layer’s presentation layer and the
        // current value of keyPath in the target layer’s presentation layer.
        unimplemented!()
    }
}

/// Interpolates two affine transforms by decomposing them into translation,
/// rotation and scale, similar to what Core Animation does, so that
/// rotations animate as rotations rather than as a matrix cross-fade.
fn interpolate_affine(a: CGAffineTransform, b: CGAffineTransform, t: f32) -> CGAffineTransform {
    fn decompose(m: CGAffineTransform) -> (f32, f32, f32, f32, f32) {
        let sx = (m.a * m.a + m.b * m.b).sqrt();
        let angle = m.b.atan2(m.a);
        let det = m.a * m.d - m.b * m.c;
        let sy = if sx != 0.0 {
            det / sx
        } else {
            (m.c * m.c + m.d * m.d).sqrt()
        };
        (m.tx, m.ty, angle, sx, sy)
    }
    let (ax, ay, aa, asx, asy) = decompose(a);
    let (bx, by, ba, bsx, bsy) = decompose(b);
    let mut da = ba - aa;
    // Shortest path
    while da > std::f32::consts::PI {
        da -= 2.0 * std::f32::consts::PI;
    }
    while da < -std::f32::consts::PI {
        da += 2.0 * std::f32::consts::PI;
    }
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    let angle = aa + da * t;
    let (sx, sy) = (lerp(asx, bsx), lerp(asy, bsy));
    let (s, c) = angle.sin_cos();
    CGAffineTransform {
        a: c * sx,
        b: s * sx,
        c: -s * sy,
        d: c * sy,
        tx: lerp(ax, bx),
        ty: lerp(ay, by),
    }
}

fn is_keyframe(env: &mut Environment, animation: id) -> bool {
    keyframe_segment(env, animation, 0.0).is_some()
}

fn id_as_option(value: id) -> Option<id> {
    if value == nil {
        None
    } else {
        Some(value)
    }
}
