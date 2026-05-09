precision highp float;

#if defined(DEBUG_FLAGS)
uniform float naru_tint;
#endif

varying vec2 naru_v_coords;
uniform vec2 naru_size;

uniform mat3 naru_input_to_geo;
uniform vec2 naru_geo_size;

uniform sampler2D naru_tex;
uniform mat3 naru_geo_to_tex;

uniform float naru_progress;
uniform float naru_clamped_progress;
uniform float naru_random_seed;

uniform float naru_alpha;
uniform float naru_scale;

