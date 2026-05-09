<h1 align="center"><img alt="naru" src="https://github.com/user-attachments/assets/07d05cd0-d5dc-4a28-9a35-51bae8f119a0"></h1>
<p align="center">A scrollable-tiling Wayland compositor.</p>
<p align="center">
    <a href="https://matrix.to/#/#naru:matrix.org"><img alt="Matrix" src="https://img.shields.io/badge/matrix-%23naru-blue?logo=matrix"></a>
    <a href="https://github.com/lechl1/naru/blob/main/LICENSE"><img alt="GitHub License" src="https://img.shields.io/github/license/lechl1/naru"></a>
    <a href="https://github.com/lechl1/naru/releases"><img alt="GitHub Release" src="https://img.shields.io/github/v/release/lechl1/naru?logo=github"></a>
</p>

<p align="center">
    <a href="https://lechl1.github.io/naru/Getting-Started.html">Getting Started</a> | <a href="https://lechl1.github.io/naru/Configuration%3A-Introduction.html">Configuration</a> | <a href="https://github.com/lechl1/naru/discussions/325">Setup&nbsp;Showcase</a>
</p>

<img width="1280" height="720" alt="naru with a few windows open" src="https://github.com/user-attachments/assets/dea5909e-1859-4aaa-9d88-d37f9663e00b" />

## About

Windows are arranged in columns on an infinite strip going to the right.
Opening a new window never causes existing windows to resize.

Every monitor has its own separate window strip.
Windows can never "overflow" onto an adjacent monitor.

Workspaces are dynamic and arranged vertically.
Every monitor has an independent set of workspaces, and there's always one empty workspace present all the way down.

The workspace arrangement is preserved across disconnecting and connecting monitors where it makes sense.
When a monitor disconnects, its workspaces will move to another monitor, but upon reconnection they will move back to the original monitor.

## Features

- Built from the ground up for scrollable tiling
- [Dynamic workspaces](https://lechl1.github.io/naru/Workspaces.html) like in GNOME
- An [Overview](https://github.com/user-attachments/assets/379a5d1f-acdb-4c11-b36c-e85fd91f0995) that zooms out workspaces and windows
- Built-in screenshot UI
- Monitor and window screencasting through xdg-desktop-portal-gnome
    - You can [block out](https://lechl1.github.io/naru/Configuration%3A-Window-Rules.html#block-out-from) sensitive windows from screencasts
    - [Dynamic cast target](https://lechl1.github.io/naru/Screencasting.html#dynamic-screencast-target) that can change what it shows on the go
- [Touchpad](https://github.com/lechl1/naru/assets/1794388/946a910e-9bec-4cd1-a923-4a9421707515) and [mouse](https://github.com/lechl1/naru/assets/1794388/8464e65d-4bf2-44fa-8c8e-5883355bd000) gestures
- Group windows into [tabs](https://lechl1.github.io/naru/Tabs.html)
- Configurable layout: gaps, borders, struts, window sizes
- [Gradient borders](https://lechl1.github.io/naru/Configuration%3A-Layout.html#gradients) with Oklab and Oklch support
- [Background blur](https://lechl1.github.io/naru/Window-Effects.html) for windows and layer-shell surfaces
- [Animations](https://github.com/lechl1/naru/assets/1794388/ce178da2-af9e-4c51-876f-8709c241d95e) with support for [custom shaders](https://github.com/lechl1/naru/assets/1794388/27a238d6-0a22-4692-b794-30dc7a626fad)
- Live-reloading config
- Works with [screen readers](https://lechl1.github.io/naru/Accessibility.html)

## Video Demo

https://github.com/lechl1/naru/assets/1794388/bce834b0-f205-434e-a027-b373495f9729

Also check out these videos that showcase a lot of the naru functionality:

- [Naru Is My New Favorite Wayland Compositor](https://www.youtube.com/watch?v=DeYx2exm04M) by Brodie Robertson
- [How Is naru This Good? Live Demo + Config](https://www.youtube.com/watch?v=7XmD5UyyhZQ) by Nick Janetakis

## Status

Naru is stable for day-to-day use and does most things expected of a Wayland compositor.
Many people are daily-driving naru, and are happy to help in our [Matrix channel].

Give it a try!
Follow the instructions on the [Getting Started](https://lechl1.github.io/naru/Getting-Started.html) page.
Grab a desktop shell like [DankMaterialShell] or [Noctalia] (or build a more traditional setup): naru by itself is not a complete desktop environment.
Also check out [awesome-naru], a list of naru-related links and projects.

Here are some points you may have questions about:

- **Multi-monitor**: yes, a core part of the design from the very start. Mixed DPI works.
- **Fractional scaling**: yes, plus all naru UI stays pixel-perfect.
- **NVIDIA**: seems to work fine.
- **Floating windows**: yes, starting from naru 25.01.
- **Input devices**: naru supports tablets, touchpads, and touchscreens.
You can map the tablet to a specific monitor, or use [OpenTabletDriver].
We have touchpad gestures, but no touchscreen gestures yet.
- **Wlr protocols**: yes, we have most of the important ones like layer-shell, gamma-control, screencopy.
You can check on [wayland.app](https://wayland.app) at the bottom of each protocol's page.
- **Performance**: while I run naru on beefy machines, I try to stay conscious of performance.
I've seen someone use it fine on an Eee PC 900 from 2008, of all things.
- **Xwayland**: [integrated](https://lechl1.github.io/naru/Xwayland.html#using-xwayland-satellite) via xwayland-satellite starting from naru 25.08.

## Media

[naru: Making a Wayland compositor in Rust](https://youtu.be/Kmz8ODolnDg?list=PLRdS-n5seLRqrmWDQY4KDqtRMfIwU0U3T) · *December 2024*

My talk from the 2024 Moscow RustCon about naru, and how I do randomized property testing and profiling, and measure input latency.
The talk is in Russian, but I prepared full English subtitles that you can find in YouTube's subtitle language selector.

[An interview with Ivan, the developer behind Naru](https://www.trommelspeicher.de/podcast/special_the_developer_behind_naru) · *June 2025*

An interview by a German tech podcast Das Triumvirat (in English).
We talk about naru development and history, and my experience building and maintaining naru.

[A tour of the naru scrolling-tiling Wayland compositor](https://lwn.net/Articles/1025866/) · *July 2025*

An LWN article with a nice overview and introduction to naru.

## Contributing

If you'd like to help with naru, there are plenty of both coding- and non-coding-related ways to do so.
See [CONTRIBUTING.md](https://github.com/lechl1/naru/blob/main/CONTRIBUTING.md) for an overview.

## Inspiration

Naru is heavily inspired by [PaperWM] which implements scrollable tiling on top of GNOME Shell.

One of the reasons that prompted me to try writing my own compositor is being able to properly separate the monitors.
Being a GNOME Shell extension, PaperWM has to work against Shell's global window coordinate space to prevent windows from overflowing.

## Tile Scrollably Elsewhere

Here are some other projects which implement a similar workflow:

- [PaperWM]: scrollable tiling on top of GNOME Shell.
- [karousel]: scrollable tiling on top of KDE.
- [scroll](https://github.com/dawsers/scroll) and [papersway]: scrollable tiling on top of sway/i3.
- Hyprland has a built-in [scrolling layout](https://wiki.hypr.land/Configuring/Layouts/Scrolling-Layout/).
- [Paneru] and [PaperWM.spoon]: scrollable tiling on top of macOS.

## Contact

Our main communication channel is a Matrix chat, feel free to join and ask a question: https://matrix.to/#/#naru:matrix.org

We also have a community Discord server: https://discord.gg/vT8Sfjy7sx

[PaperWM]: https://github.com/paperwm/PaperWM
[waybar]: https://github.com/Alexays/Waybar
[fuzzel]: https://codeberg.org/dnkl/fuzzel
[awesome-naru]: https://github.com/lechl1/awesome-naru
[karousel]: https://github.com/peterfajdiga/karousel
[papersway]: https://spwhitton.name/tech/code/papersway/
[Paneru]: https://github.com/karinushka/paneru
[PaperWM.spoon]: https://github.com/mogenson/PaperWM.spoon
[Matrix channel]: https://matrix.to/#/#naru:matrix.org
[OpenTabletDriver]: https://opentabletdriver.net/
[DankMaterialShell]: https://danklinux.com/
[Noctalia]: https://noctalia.dev/
