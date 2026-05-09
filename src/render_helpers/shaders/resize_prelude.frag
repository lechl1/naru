precision highp float;

#if defined(DEBUG_FLAGS)
uniform float naru_tint;
#endif

varying vec2 naru_v_coords;
uniform vec2 naru_size;

uniform mat3 naru_input_to_curr_geo;
uniform mat3 naru_curr_geo_to_prev_geo;
uniform mat3 naru_curr_geo_to_next_geo;
uniform vec2 naru_curr_geo_size;

uniform sampler2D naru_tex_prev;
uniform mat3 naru_geo_to_tex_prev;

uniform sampler2D naru_tex_next;
uniform mat3 naru_geo_to_tex_next;

uniform float naru_progress;
uniform float naru_clamped_progress;

uniform vec4 naru_corner_radius;
uniform float naru_clip_to_geometry;

uniform float naru_alpha;
uniform float naru_scale;

float naru_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);
