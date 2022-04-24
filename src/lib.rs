use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use parts::Parts;
use pointer::PointerUserData;
use smithay_client_toolkit::reexports::client;

use client::protocol::{wl_compositor, wl_seat, wl_shm, wl_subcompositor, wl_surface};
use client::{Attached, DispatchData};
use tiny_skia::{
    Color, FillRule, Paint, Path, PathBuilder, Pixmap, Point, Rect, Shader, Stroke, Transform,
};

use smithay_client_toolkit::{
    seat::pointer::{ThemeManager, ThemeSpec, ThemedPointer},
    shm::AutoMemPool,
    window::{ButtonState, Frame, FrameRequest, State, WindowState},
};

mod theme;
use theme::{ColorTheme, BORDER_COLOR, BORDER_SIZE, HEADER_SIZE};

mod buttons;
use buttons::{ButtonKind, Buttons};

mod parts;
mod pointer;
mod surface;

/*
 * Utilities
 */

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Location {
    None,
    Head,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    TopLeft,
    Button(ButtonKind),
}

/*
 * The core frame
 */

pub struct Inner {
    parts: Parts,
    size: (u32, u32),
    resizable: bool,
    theme_over_surface: bool,
    implem: Box<dyn FnMut(FrameRequest, u32, DispatchData)>,
    maximized: bool,
    fullscreened: bool,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("parts", &self.parts)
            .field("size", &self.size)
            .field("resizable", &self.resizable)
            .field("theme_over_surface", &self.theme_over_surface)
            .field(
                "implem",
                &"FnMut(FrameRequest, u32, DispatchData) -> { ... }",
            )
            .field("maximized", &self.maximized)
            .field("fullscreened", &self.fullscreened)
            .finish()
    }
}

fn precise_location(buttons: &Buttons, old: Location, width: u32, x: f64, y: f64) -> Location {
    match old {
        Location::Head
        | Location::Button(_)
        | Location::Top
        | Location::TopLeft
        | Location::TopRight => match buttons.find_button(x, y) {
            Location::Head => {
                if y <= f64::from(BORDER_SIZE) {
                    if x <= f64::from(BORDER_SIZE) {
                        Location::TopLeft
                    } else if x >= f64::from(width + BORDER_SIZE) {
                        Location::TopRight
                    } else {
                        Location::Top
                    }
                } else {
                    Location::Head
                }
            }
            other => other,
        },

        Location::Bottom | Location::BottomLeft | Location::BottomRight => {
            if x <= f64::from(BORDER_SIZE) {
                Location::BottomLeft
            } else if x >= f64::from(width + BORDER_SIZE) {
                Location::BottomRight
            } else {
                Location::Bottom
            }
        }

        other => other,
    }
}

/// A simple set of decorations
#[derive(Debug)]
pub struct AdwaitaFrame {
    base_surface: wl_surface::WlSurface,
    compositor: Attached<wl_compositor::WlCompositor>,
    subcompositor: Attached<wl_subcompositor::WlSubcompositor>,
    inner: Rc<RefCell<Inner>>,
    pool: AutoMemPool,
    active: WindowState,
    hidden: bool,
    pointers: Vec<ThemedPointer>,
    themer: ThemeManager,
    surface_version: u32,

    buttons: Rc<RefCell<Buttons>>,
    colors: ColorTheme,
}

impl Frame for AdwaitaFrame {
    type Error = ::std::io::Error;
    type Config = ();
    fn init(
        base_surface: &wl_surface::WlSurface,
        compositor: &Attached<wl_compositor::WlCompositor>,
        subcompositor: &Attached<wl_subcompositor::WlSubcompositor>,
        shm: &Attached<wl_shm::WlShm>,
        theme_manager: Option<ThemeManager>,
        implementation: Box<dyn FnMut(FrameRequest, u32, DispatchData)>,
    ) -> Result<AdwaitaFrame, ::std::io::Error> {
        let (themer, theme_over_surface) = if let Some(theme_manager) = theme_manager {
            (theme_manager, false)
        } else {
            (
                ThemeManager::init(ThemeSpec::System, compositor.clone(), shm.clone()),
                true,
            )
        };

        let inner = Rc::new(RefCell::new(Inner {
            parts: Parts::default(),
            size: (1, 1),
            resizable: true,
            implem: implementation,
            theme_over_surface,
            maximized: false,
            fullscreened: false,
        }));

        let pool = AutoMemPool::new(shm.clone())?;

        Ok(AdwaitaFrame {
            base_surface: base_surface.clone(),
            compositor: compositor.clone(),
            subcompositor: subcompositor.clone(),
            inner,
            pool,
            active: WindowState::Inactive,
            hidden: true,
            pointers: Vec::new(),
            themer,
            surface_version: compositor.as_ref().version(),
            buttons: Default::default(),
            colors: Default::default(),
        })
    }

    fn new_seat(&mut self, seat: &Attached<wl_seat::WlSeat>) {
        let inner = self.inner.clone();

        let buttons = self.buttons.clone();
        let pointer = self.themer.theme_pointer_with_impl(
            seat,
            move |event, pointer: ThemedPointer, ddata: DispatchData| {
                let data: &RefCell<PointerUserData> = pointer.as_ref().user_data().get().unwrap();
                let mut data = data.borrow_mut();
                let mut inner = inner.borrow_mut();
                data.event(event, &mut inner, &buttons.borrow(), &pointer, ddata);
            },
        );
        pointer
            .as_ref()
            .user_data()
            .set(|| RefCell::new(PointerUserData::new(seat.detach())));
        self.pointers.push(pointer);
    }

    fn remove_seat(&mut self, seat: &wl_seat::WlSeat) {
        self.pointers.retain(|pointer| {
            let user_data = pointer
                .as_ref()
                .user_data()
                .get::<RefCell<PointerUserData>>()
                .unwrap();
            let guard = user_data.borrow_mut();
            if &guard.seat == seat {
                pointer.release();
                false
            } else {
                true
            }
        });
    }

    fn set_states(&mut self, states: &[State]) -> bool {
        let mut inner = self.inner.borrow_mut();
        let mut need_redraw = false;

        // Process active.
        let new_active = if states.contains(&State::Activated) {
            WindowState::Active
        } else {
            WindowState::Inactive
        };
        need_redraw |= new_active != self.active;
        self.active = new_active;

        // Process maximized.
        let new_maximized = states.contains(&State::Maximized);
        need_redraw |= new_maximized != inner.maximized;
        inner.maximized = new_maximized;

        // Process fullscreened.
        let new_fullscreened = states.contains(&State::Fullscreen);
        need_redraw |= new_fullscreened != inner.fullscreened;
        inner.fullscreened = new_fullscreened;

        need_redraw
    }

    fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
        let mut inner = self.inner.borrow_mut();
        if !self.hidden {
            inner.parts.add_decorations(
                &self.base_surface,
                &self.compositor,
                &self.subcompositor,
                self.inner.clone(),
            );
        } else {
            inner.parts.remove_decorations();
        }
    }

    fn set_resizable(&mut self, resizable: bool) {
        self.inner.borrow_mut().resizable = resizable;
    }

    fn resize(&mut self, newsize: (u32, u32)) {
        self.inner.borrow_mut().size = newsize;
        self.buttons
            .borrow_mut()
            .arrange(newsize.0 + BORDER_SIZE * 2);
    }

    fn redraw(&mut self) {
        let inner = self.inner.borrow_mut();

        // Don't draw borders if the frame explicitly hidden or fullscreened.
        if self.hidden || inner.fullscreened {
            inner.parts.hide_decorations();
            return;
        }

        // `parts` can't be empty here, since the initial state for `self.hidden` is true, and
        // they will be created once `self.hidden` will become `false`.
        let parts = &inner.parts;

        let (width, height) = inner.size;

        if let Some(decoration) = parts.decoration() {
            // Use header scale for all the thing.
            let header_scale = decoration.header.scale();
            self.buttons.borrow_mut().update_scale(header_scale);

            let left_scale = decoration.left.scale();
            let right_scale = decoration.right.scale();
            let bottom_scale = decoration.bottom.scale();

            // dbg!(
            //     header_scale,
            //     top_scale,
            //     left_scale,
            //     right_scale,
            //     bottom_scale
            // );

            let (header_width, header_height) = self.buttons.borrow().scaled_size();
            let header_height = header_height + BORDER_SIZE * header_scale;

            {
                // Create the buffers and draw

                let colors = if self.active == WindowState::Active {
                    &self.colors.active
                } else {
                    &self.colors.inactive
                };
                let border_paint = colors.border_paint();

                // -> head-subsurface
                if let Ok((canvas, buffer)) = self.pool.buffer(
                    header_width as i32,
                    header_height as i32,
                    4 * header_width as i32,
                    wl_shm::Format::Argb8888,
                ) {
                    draw_buttons(
                        canvas,
                        header_width,
                        header_height,
                        header_scale as f32,
                        inner.resizable,
                        self.active,
                        &self.colors,
                        &self.buttons.borrow(),
                        &self
                            .pointers
                            .iter()
                            .flat_map(|p| {
                                if p.as_ref().is_alive() {
                                    let data: &RefCell<PointerUserData> =
                                        p.as_ref().user_data().get().unwrap();
                                    Some(data.borrow().location)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<Location>>(),
                    );

                    // decoration
                    //     .header
                    //     .subsurface
                    //     .set_position(-(BORDER_SIZE as i32), -(HEADER_SIZE as i32));
                    decoration.header.subsurface.set_position(
                        -(BORDER_SIZE as i32),
                        -(HEADER_SIZE as i32 + BORDER_SIZE as i32),
                    );
                    decoration.header.surface.attach(Some(&buffer), 0, 0);
                    if self.surface_version >= 4 {
                        decoration.header.surface.damage_buffer(
                            0,
                            0,
                            header_width as i32,
                            header_height as i32,
                        );
                    } else {
                        // surface is old and does not support damage_buffer, so we damage
                        // in surface coordinates and hope it is not rescaled
                        decoration
                            .header
                            .surface
                            .damage(0, 0, width as i32, HEADER_SIZE as i32);
                    }
                    decoration.header.surface.commit();
                }

                if inner.maximized {
                    // Don't draw the borders.
                    decoration.hide_borders();
                    return;
                }

                let w = ((width + 2 * BORDER_SIZE) * bottom_scale) as i32;
                let h = (BORDER_SIZE * bottom_scale) as i32;
                // -> bottom-subsurface
                if let Ok((canvas, buffer)) = self.pool.buffer(
                    w,
                    h,
                    (4 * bottom_scale * (width + 2 * BORDER_SIZE)) as i32,
                    wl_shm::Format::Argb8888,
                ) {
                    let mut pixmap = Pixmap::new(w as u32, h as u32).unwrap();

                    let size = 1.0;
                    let x = BORDER_SIZE as f32 * bottom_scale as f32 - 1.0;
                    pixmap.fill_rect(
                        Rect::from_xywh(
                            x,
                            0.0,
                            w as f32 - BORDER_SIZE as f32 * 2.0 * bottom_scale as f32 + 2.0,
                            size,
                        )
                        .unwrap(),
                        &border_paint,
                        Transform::identity(),
                        None,
                    );

                    let data = pixmap.data();
                    for (id, pixel) in canvas.iter_mut().enumerate() {
                        *pixel = data[id];
                    }

                    decoration
                        .bottom
                        .subsurface
                        .set_position(-(BORDER_SIZE as i32), height as i32);
                    decoration.bottom.surface.attach(Some(&buffer), 0, 0);
                    if self.surface_version >= 4 {
                        decoration.bottom.surface.damage_buffer(
                            0,
                            0,
                            ((width + 2 * BORDER_SIZE) * bottom_scale) as i32,
                            (BORDER_SIZE * bottom_scale) as i32,
                        );
                    } else {
                        // surface is old and does not support damage_buffer, so we damage
                        // in surface coordinates and hope it is not rescaled
                        decoration.bottom.surface.damage(
                            0,
                            0,
                            (width + 2 * BORDER_SIZE) as i32,
                            BORDER_SIZE as i32,
                        );
                    }
                    decoration.bottom.surface.commit();
                }

                let w = (BORDER_SIZE * left_scale) as i32;
                let h = (height * left_scale) as i32;
                // -> left-subsurface
                if let Ok((canvas, buffer)) = self.pool.buffer(
                    w,
                    h,
                    4 * (BORDER_SIZE * left_scale) as i32,
                    wl_shm::Format::Argb8888,
                ) {
                    let mut bg = Paint::default();
                    bg.set_color_rgba8(255, 0, 0, 255);

                    let mut pixmap = Pixmap::new(w as u32, h as u32).unwrap();

                    let size = 1.0;
                    pixmap.fill_rect(
                        Rect::from_xywh(w as f32 - size, 0.0, w as f32, h as f32).unwrap(),
                        &border_paint,
                        Transform::identity(),
                        None,
                    );

                    let data = pixmap.data();
                    for (id, pixel) in canvas.iter_mut().enumerate() {
                        *pixel = data[id];
                    }

                    decoration
                        .left
                        .subsurface
                        .set_position(-(BORDER_SIZE as i32), 0);
                    decoration.left.surface.attach(Some(&buffer), 0, 0);
                    if self.surface_version >= 4 {
                        decoration.left.surface.damage_buffer(0, 0, w, h);
                    } else {
                        // surface is old and does not support damage_buffer, so we damage
                        // in surface coordinates and hope it is not rescaled
                        decoration.left.surface.damage(
                            0,
                            0,
                            BORDER_SIZE as i32,
                            (height + HEADER_SIZE) as i32,
                        );
                    }
                    decoration.left.surface.commit();
                }

                let w = (BORDER_SIZE * right_scale) as i32;
                let h = (height * right_scale) as i32;
                // -> right-subsurface
                if let Ok((canvas, buffer)) = self.pool.buffer(
                    w,
                    h,
                    4 * (BORDER_SIZE * right_scale) as i32,
                    wl_shm::Format::Argb8888,
                ) {
                    let mut bg = Paint::default();
                    bg.set_color_rgba8(255, 0, 0, 255);

                    let mut pixmap = Pixmap::new(w as u32, h as u32).unwrap();

                    let size = 1.0;
                    pixmap.fill_rect(
                        Rect::from_xywh(0.0, 0.0, size, h as f32).unwrap(),
                        &border_paint,
                        Transform::identity(),
                        None,
                    );

                    let data = pixmap.data();
                    for (id, pixel) in canvas.iter_mut().enumerate() {
                        *pixel = data[id];
                    }

                    decoration.right.subsurface.set_position(width as i32, 0);
                    decoration.right.surface.attach(Some(&buffer), 0, 0);
                    if self.surface_version >= 4 {
                        decoration.right.surface.damage_buffer(0, 0, w, h);
                    } else {
                        // surface is old and does not support damage_buffer, so we damage
                        // in surface coordinates and hope it is not rescaled
                        decoration
                            .right
                            .surface
                            .damage(0, 0, BORDER_SIZE as i32, height as i32);
                    }
                    decoration.right.surface.commit();
                }
            }
        }
    }

    fn subtract_borders(&self, width: i32, height: i32) -> (i32, i32) {
        if self.hidden || self.inner.borrow().fullscreened {
            (width, height)
        } else {
            (width, height - HEADER_SIZE as i32)
        }
    }

    fn add_borders(&self, width: i32, height: i32) -> (i32, i32) {
        if self.hidden || self.inner.borrow().fullscreened {
            (width, height)
        } else {
            (width, height + HEADER_SIZE as i32)
        }
    }

    fn location(&self) -> (i32, i32) {
        if self.hidden || self.inner.borrow().fullscreened {
            (0, 0)
        } else {
            (0, -(HEADER_SIZE as i32))
        }
    }

    fn set_config(&mut self, _config: ()) {}

    fn set_title(&mut self, _title: String) {}
}

impl Drop for AdwaitaFrame {
    fn drop(&mut self) {
        for ptr in self.pointers.drain(..) {
            if ptr.as_ref().version() >= 3 {
                ptr.release();
            }
        }
    }
}

fn rounded_headerbar(
    mut pb: PathBuilder,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
) -> Option<Path> {
    use std::f32::consts::FRAC_1_SQRT_2;

    let mut cursor = Point::from_xy(x, y);

    // !!!
    // This code is heavily "inspired" by https://gitlab.com/snakedye/snui/
    // So technically it should be licensed under MPL-2.0, sorry about that 🥺 👉👈
    // !!!

    // Positioning the cursor
    cursor.y += radius;
    pb.move_to(cursor.x, cursor.y);

    // Drawing the outline
    pb.cubic_to(
        cursor.x,
        cursor.y,
        cursor.x,
        cursor.y - FRAC_1_SQRT_2 * radius,
        {
            cursor.x += radius;
            cursor.x
        },
        {
            cursor.y -= radius;
            cursor.y
        },
    );
    pb.line_to(
        {
            cursor.x = x + width - radius;
            cursor.x
        },
        cursor.y,
    );
    pb.cubic_to(
        cursor.x,
        cursor.y,
        cursor.x + FRAC_1_SQRT_2 * radius,
        cursor.y,
        {
            cursor.x += radius;
            cursor.x
        },
        {
            cursor.y += radius;
            cursor.y
        },
    );
    pb.line_to(cursor.x, {
        cursor.y = y + height;
        cursor.y
    });
    pb.line_to(
        {
            cursor.x = x;
            cursor.x
        },
        cursor.y,
    );

    pb.close();

    pb.finish()
}

fn draw_buttons(
    canvas: &mut [u8],
    w: u32,
    h: u32,
    scale: f32,
    maximizable: bool,
    state: WindowState,
    colors: &ColorTheme,
    buttons: &Buttons,
    mouses: &[Location],
) {
    let border_size = BORDER_SIZE as f32 * scale;

    let margin_top = border_size;
    let margin_left = border_size;

    let colors = if state == WindowState::Active {
        &colors.active
    } else {
        &colors.inactive
    };

    let mut pixmap = Pixmap::new(w, h).unwrap();

    {
        let h = h as f32;
        let w = w as f32;

        let r = 10.0 * scale;

        let margin_left = margin_left - 1.0;
        let w = w - margin_left * 2.0;

        let corner_l = {
            let pb = PathBuilder::new();

            let x = margin_left;
            let y = margin_top;

            rounded_headerbar(pb, x, y, w, h, r).unwrap()
        };

        let headerbar_paint = colors.headerbar_paint();
        pixmap.fill_path(
            &corner_l,
            &headerbar_paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );

        // Line

        let mut line = Paint::default();
        line.set_color_rgba8(220, 220, 220, 255);
        line.anti_alias = false;

        pixmap.fill_rect(
            Rect::from_xywh(margin_left, h - 1.0, w, h).unwrap(),
            &line,
            Transform::identity(),
            None,
        );
    }

    if buttons.close.x() > margin_left {
        buttons
            .close
            .draw_close(scale, state, colors, mouses, &mut pixmap);
    }

    if buttons.maximize.x() > margin_left {
        buttons
            .maximize
            .draw_maximize(scale, state, colors, mouses, maximizable, &mut pixmap);
    }

    if buttons.minimize.x() > margin_left {
        buttons
            .minimize
            .draw_minimize(scale, state, colors, mouses, &mut pixmap);
    }

    let buff = pixmap.data();

    for (id, pixel) in canvas.iter_mut().enumerate() {
        *pixel = buff[id];
    }
}