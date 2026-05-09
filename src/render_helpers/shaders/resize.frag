vec4 resize_color(vec3 coords_curr_geo, vec3 size_curr_geo) {
    vec3 coords_tex_prev = naru_geo_to_tex_prev * coords_curr_geo;
    vec4 color_prev = texture2D(naru_tex_prev, coords_tex_prev.st);

    vec3 coords_tex_next = naru_geo_to_tex_next * coords_curr_geo;
    vec4 color_next = texture2D(naru_tex_next, coords_tex_next.st);

    vec4 color = mix(color_prev, color_next, naru_clamped_progress);
    return color;
}
