#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// A linear opacity fade along the x axis, in texture coordinates:
//   cutoff.x = v_coords.x at which the content is fully transparent,
//   cutoff.y = v_coords.x at which it is fully opaque.
// Either ordering is valid: for a left-side panel cutoff.x < cutoff.y (fade in
// from the panel edge rightward), for a right-side panel cutoff.x > cutoff.y.
uniform vec2 cutoff;

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    float denom = cutoff.y - cutoff.x;
    if (abs(denom) > 0.0001) {
        float fade = clamp((v_coords.x - cutoff.x) / denom, 0.0, 1.0);
        color = color * fade;
    }

    // Apply final alpha and tint.
    color = color * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
