// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#version 450

// LINT.IfChange(push_constants)
layout(push_constant) uniform Params {
  vec2 half_extent;     // Image size / 2, in pixels.
  vec2 step;            // Complex units per pixel, horizontally and vertically.
  vec2 c;               // The Julia set's parameter.
  uint max_iterations;
  uint palette[21];     // Colors evenly spaced from t = 0 to 1, packed as by packUnorm4x8.
}
params;
// LINT.ThenChange(//src/graphics/tests/vkfractal/vkfractal.cc:push_constants)

// Every pixel runs max_iterations iterations instead of stopping when it escapes.
layout(constant_id = 0) const bool kFixedIterations = false;

layout(location = 0) out vec4 out_color;

// A larger escape radius than 2 keeps the smooth iteration count continuous between iterations.
const float kEscapeRadiusSquared = 256.0;
// Smooth iteration counts from 0 to this map logarithmically onto the palette.
const float kPaletteIterations = 256.0;

vec3 Palette(float t) {
  float x = clamp(t, 0.0, 1.0) * float(params.palette.length() - 1);
  uint i = min(uint(x), uint(params.palette.length() - 2));
  return mix(unpackUnorm4x8(params.palette[i]).rgb, unpackUnorm4x8(params.palette[i + 1u]).rgb,
             x - float(i));
}

void main() {
  // The image is centered on 0. Imaginary values increase upward on screen.
  vec2 z = (gl_FragCoord.xy - params.half_extent) * vec2(params.step.x, -params.step.y);

  uint i = 0u;
  if (kFixedIterations) {
    // Keep iterating after escaping, with z frozen at its escaped value, so every pixel costs
    // the same and the image matches early exit.
    i = params.max_iterations;
    for (uint n = 0u; n < params.max_iterations; ++n) {
      vec2 next = vec2(z.x * z.x - z.y * z.y, 2.0 * z.x * z.y) + params.c;
      bool escaped = i < params.max_iterations;
      z = escaped ? z : next;
      i = (!escaped && dot(z, z) > kEscapeRadiusSquared) ? n : i;
    }
  } else {
    for (; i < params.max_iterations; ++i) {
      z = vec2(z.x * z.x - z.y * z.y, 2.0 * z.x * z.y) + params.c;
      if (dot(z, z) > kEscapeRadiusSquared) {
        break;
      }
    }
  }

  // Points that never escape are inside the set and take the palette's first color.
  float t = 0.0;
  if (i < params.max_iterations) {
    float nu = float(i) + 1.0 - log2(log2(dot(z, z)) / log2(kEscapeRadiusSquared));
    t = log2(1.0 + max(nu, 0.0)) / log2(1.0 + kPaletteIterations);
  }
  out_color = vec4(Palette(t), 1.0);
}
