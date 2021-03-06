use crate::{camera::*, config::Config, controls::*, gui::Gui, loader::*, renderer::*};
use ash::{version::DeviceV1_0, vk, Device};
use environment::*;
use model::{Model, PlaybackMode};
use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc, time::Instant};
use vulkan::*;
use winit::{dpi::LogicalSize, Event, EventsLoop, Window, WindowBuilder, WindowEvent};

pub const MAX_FRAMES_IN_FLIGHT: u32 = 2;

pub struct Viewer {
    config: Config,
    events_loop: EventsLoop,
    window: Window,
    resize_dimensions: Option<[u32; 2]>,
    run: bool,

    camera: Camera,
    input_state: InputState,
    model: Option<Rc<RefCell<Model>>>,

    gui: Gui,

    context: Arc<Context>,
    swapchain_properties: SwapchainProperties,
    simple_render_pass: SimpleRenderPass,
    swapchain: Swapchain,

    renderer: Renderer,
    command_buffers: Vec<vk::CommandBuffer>,
    in_flight_frames: InFlightFrames,

    loader: Loader,
}

impl Viewer {
    pub fn new<P: AsRef<Path>>(config: Config, enable_debug: bool, path: Option<P>) -> Self {
        log::debug!("Creating application.");

        let resolution = [config.resolution().width(), config.resolution().height()];

        let events_loop = EventsLoop::new();
        let window = WindowBuilder::new()
            .with_title("GLTF Viewer")
            .with_dimensions(LogicalSize::new(
                f64::from(resolution[0]),
                f64::from(resolution[1]),
            ))
            .build(&events_loop)
            .unwrap();

        let mut gui = Gui::new(&window);

        let context = Arc::new(Context::new(&window, enable_debug));

        let swapchain_support_details = SwapchainSupportDetails::new(
            context.physical_device(),
            context.surface(),
            context.surface_khr(),
        );
        let swapchain_properties =
            swapchain_support_details.get_ideal_swapchain_properties(resolution, config.vsync());
        let depth_format = Self::find_depth_format(&context);
        let msaa_samples = context.get_max_usable_sample_count(config.msaa());
        log::debug!("msaa: {:?} - preferred was {}", msaa_samples, config.msaa());

        let simple_render_pass =
            SimpleRenderPass::create(Arc::clone(&context), swapchain_properties.format.format);

        let swapchain = Swapchain::create(
            Arc::clone(&context),
            swapchain_support_details,
            resolution,
            config.vsync(),
            &simple_render_pass,
        );

        let environment =
            Environment::new(&context, config.env().path(), config.env().resolution());

        let renderer = Renderer::create(
            Arc::clone(&context),
            depth_format,
            msaa_samples,
            swapchain_properties,
            &simple_render_pass,
            environment,
            gui.get_context(),
        );

        let command_buffers = Self::allocate_command_buffers(&context, swapchain.image_count());

        let in_flight_frames = Self::create_sync_objects(context.device());

        let loader = Loader::new(Arc::new(context.new_thread()));
        if let Some(p) = path {
            loader.load(p.as_ref().to_path_buf());
        }

        Self {
            events_loop,
            window,
            config,
            resize_dimensions: None,
            run: true,
            camera: Default::default(),
            input_state: Default::default(),
            model: None,
            gui,
            context,
            swapchain_properties,
            simple_render_pass,
            swapchain,
            renderer,
            command_buffers,
            in_flight_frames,
            loader,
        }
    }

    fn find_depth_format(context: &Context) -> vk::Format {
        let candidates = vec![
            vk::Format::D32_SFLOAT,
            vk::Format::D32_SFLOAT_S8_UINT,
            vk::Format::D24_UNORM_S8_UINT,
        ];
        context
            .find_supported_format(
                &candidates,
                vk::ImageTiling::OPTIMAL,
                vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT,
            )
            .expect("Failed to find a supported depth format")
    }

    fn allocate_command_buffers(context: &Context, count: usize) -> Vec<vk::CommandBuffer> {
        let allocate_info = vk::CommandBufferAllocateInfo::builder()
            .command_pool(context.general_command_pool())
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(count as _);

        unsafe {
            context
                .device()
                .allocate_command_buffers(&allocate_info)
                .unwrap()
        }
    }

    fn create_sync_objects(device: &Device) -> InFlightFrames {
        let mut sync_objects_vec = Vec::new();
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let image_available_semaphore = {
                let semaphore_info = vk::SemaphoreCreateInfo::builder();
                unsafe { device.create_semaphore(&semaphore_info, None).unwrap() }
            };

            let render_finished_semaphore = {
                let semaphore_info = vk::SemaphoreCreateInfo::builder();
                unsafe { device.create_semaphore(&semaphore_info, None).unwrap() }
            };

            let in_flight_fence = {
                let fence_info =
                    vk::FenceCreateInfo::builder().flags(vk::FenceCreateFlags::SIGNALED);
                unsafe { device.create_fence(&fence_info, None).unwrap() }
            };

            let sync_objects = SyncObjects {
                image_available_semaphore,
                render_finished_semaphore,
                fence: in_flight_fence,
            };
            sync_objects_vec.push(sync_objects)
        }

        InFlightFrames::new(sync_objects_vec)
    }

    pub fn run(&mut self) {
        log::debug!("Running application.");
        let mut time = Instant::now();
        loop {
            let new_time = Instant::now();
            let delta_s = ((new_time - time).as_nanos() as f64) / 1_000_000_000.0;
            time = new_time;

            self.process_event();
            if !self.run {
                break;
            }

            self.load_new_model();
            self.update_model(delta_s as f32);
            self.update_camera();
            self.update_renderer_settings();
            self.draw_frame();
        }
        unsafe { self.context.device().device_wait_idle().unwrap() };
    }

    /// Process the events from the `EventsLoop` and return whether the
    /// main loop should stop.
    fn process_event(&mut self) {
        if !self.run {
            return;
        }

        let mut run = true;
        let mut resize_dimensions = None;
        let mut path_to_load = None;
        let mut input_state = self.input_state;
        input_state.reset();

        let gui = &mut self.gui;
        let window = &self.window;

        self.events_loop.poll_events(|event| {
            gui.handle_event(window, &event);
            input_state = input_state.update(&event);
            if let Event::WindowEvent { event, .. } = event {
                match event {
                    WindowEvent::CloseRequested => run = false,
                    WindowEvent::Resized(LogicalSize { width, height }) => {
                        resize_dimensions = Some([width as u32, height as u32]);
                    }
                    WindowEvent::DroppedFile(path) => {
                        log::debug!("File dropped: {:?}", path);
                        path_to_load = Some(path);
                    }
                    _ => {}
                }
            }
        });

        self.gui.prepare_frame(window);

        self.resize_dimensions = resize_dimensions;
        if path_to_load.is_some() {
            self.loader.load(path_to_load.as_ref().cloned().unwrap());
        }
        self.input_state = input_state;
        self.run = run;
    }

    fn load_new_model(&mut self) {
        if let Some(model) = self.loader.get_model() {
            self.gui.set_model_metadata(model.metadata().clone());
            self.model.take();

            self.context.graphics_queue_wait_idle();
            let model = Rc::new(RefCell::new(model));
            self.renderer.set_model(&model);
            self.model = Some(model);
        }
    }

    fn update_model(&mut self, delta_s: f32) {
        if let Some(model) = self.model.as_ref() {
            let mut model = model.borrow_mut();

            if self.gui.should_toggle_animation() {
                model.toggle_animation();
            } else if self.gui.should_stop_animation() {
                model.stop_animation();
            } else if self.gui.should_reset_animation() {
                model.reset_animation();
            } else {
                let playback_mode = if self.gui.is_infinite_animation_checked() {
                    PlaybackMode::LOOP
                } else {
                    PlaybackMode::ONCE
                };

                model.set_animation_playback_mode(playback_mode);
                model.set_current_animation(self.gui.get_selected_animation());
            }
            self.gui
                .set_animation_playback_state(model.get_animation_playback_state());

            let delta_s = delta_s * self.gui.get_animation_speed();
            model.update(delta_s);
        }
    }

    fn update_camera(&mut self) {
        if self.gui.should_reset_camera() {
            self.camera = Default::default();
        }

        if self.gui.is_hovered() {
            return;
        }

        self.camera.update(&self.input_state);
        self.gui.set_camera(Some(self.camera));
    }

    fn update_renderer_settings(&mut self) {
        if let Some(emissive_intensity) = self.gui.get_new_emissive_intensity() {
            self.context.graphics_queue_wait_idle();
            self.renderer.set_emissive_intensity(emissive_intensity);
        }
        if let Some(ssao_enabled) = self.gui.get_new_ssao_enabled() {
            self.context.graphics_queue_wait_idle();
            self.renderer.enabled_ssao(ssao_enabled);
        }
        if let Some(ssao_kernel_size) = self.gui.get_new_ssao_kernel_size() {
            self.context.graphics_queue_wait_idle();
            self.renderer.set_ssao_kernel_size(ssao_kernel_size);
        }
        if let Some(ssao_radius) = self.gui.get_new_ssao_radius() {
            self.context.graphics_queue_wait_idle();
            self.renderer.set_ssao_radius(ssao_radius);
        }
        if let Some(ssao_strength) = self.gui.get_new_ssao_strength() {
            self.context.graphics_queue_wait_idle();
            self.renderer.set_ssao_strength(ssao_strength);
        }
        if let Some(tone_map_mode) = self.gui.get_new_renderer_tone_map_mode() {
            self.context.graphics_queue_wait_idle();
            self.renderer
                .set_tone_map_mode(&self.simple_render_pass, tone_map_mode);
        }
        if let Some(output_mode) = self.gui.get_new_renderer_output_mode() {
            self.context.graphics_queue_wait_idle();
            self.renderer.set_output_mode(output_mode);
        }
    }

    fn draw_frame(&mut self) {
        log::trace!("Drawing frame.");
        let sync_objects = self.in_flight_frames.next().unwrap();
        let image_available_semaphore = sync_objects.image_available_semaphore;
        let render_finished_semaphore = sync_objects.render_finished_semaphore;
        let in_flight_fence = sync_objects.fence;
        let wait_fences = [in_flight_fence];

        unsafe {
            self.context
                .device()
                .wait_for_fences(&wait_fences, true, std::u64::MAX)
                .unwrap()
        };

        let result = self
            .swapchain
            .acquire_next_image(None, Some(image_available_semaphore), None);
        let image_index = match result {
            Ok((image_index, _)) => image_index,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain();
                return;
            }
            Err(error) => panic!("Error while acquiring next image. Cause: {}", error),
        };

        unsafe { self.context.device().reset_fences(&wait_fences).unwrap() };

        self.record_command_buffer(self.command_buffers[image_index as usize], image_index as _);
        self.renderer.update_ubos(image_index as _, self.camera);

        let device = self.context.device();
        let wait_semaphores = [image_available_semaphore];
        let signal_semaphores = [render_finished_semaphore];

        // Submit command buffer
        {
            let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let command_buffers = [self.command_buffers[image_index as usize]];
            let submit_info = vk::SubmitInfo::builder()
                .wait_semaphores(&wait_semaphores)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&command_buffers)
                .signal_semaphores(&signal_semaphores)
                .build();
            let submit_infos = [submit_info];
            unsafe {
                device
                    .queue_submit(
                        self.context.graphics_queue(),
                        &submit_infos,
                        in_flight_fence,
                    )
                    .unwrap()
            };
        }

        let swapchains = [self.swapchain.swapchain_khr()];
        let images_indices = [image_index];

        {
            let present_info = vk::PresentInfoKHR::builder()
                .wait_semaphores(&signal_semaphores)
                .swapchains(&swapchains)
                .image_indices(&images_indices);
            let result = self.swapchain.present(&present_info);
            match result {
                Ok(is_suboptimal) if is_suboptimal => {
                    self.recreate_swapchain();
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.recreate_swapchain();
                }
                Err(error) => panic!("Failed to present queue. Cause: {}", error),
                _ => {}
            }

            if self.resize_dimensions.is_some() {
                self.recreate_swapchain();
            }
        }
    }

    fn record_command_buffer(&mut self, command_buffer: vk::CommandBuffer, frame_index: usize) {
        let device = self.context.device();

        unsafe {
            device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .unwrap();
        }

        // begin command buffer
        {
            let command_buffer_begin_info = vk::CommandBufferBeginInfo::builder()
                .flags(vk::CommandBufferUsageFlags::SIMULTANEOUS_USE);
            unsafe {
                device
                    .begin_command_buffer(command_buffer, &command_buffer_begin_info)
                    .unwrap()
            };
        }

        let draw_data = self.gui.render(&mut self.run, &self.window);

        self.renderer.cmd_draw(
            command_buffer,
            frame_index,
            self.swapchain.properties(),
            &self.simple_render_pass,
            self.swapchain.framebuffers()[frame_index],
            draw_data,
        );

        // End command buffer
        unsafe { device.end_command_buffer(command_buffer).unwrap() };
    }

    /// Recreates the swapchain.
    ///
    /// If the window has been resized, then the new size is used
    /// otherwise, the size of the current swapchain is used.
    ///
    /// If the window has been minimized, then the functions block until
    /// the window is maximized. This is because a width or height of 0
    /// is not legal.
    fn recreate_swapchain(&mut self) {
        log::debug!("Recreating swapchain.");

        if self.has_window_been_minimized() {
            while !self.has_window_been_maximized() {
                self.process_event();
            }
        }

        unsafe { self.context.device().device_wait_idle().unwrap() };

        self.cleanup_swapchain();

        let dimensions = self.resize_dimensions.unwrap_or([
            self.swapchain.properties().extent.width,
            self.swapchain.properties().extent.height,
        ]);

        let swapchain_support_details = SwapchainSupportDetails::new(
            self.context.physical_device(),
            self.context.surface(),
            self.context.surface_khr(),
        );
        let swapchain_properties = swapchain_support_details
            .get_ideal_swapchain_properties(dimensions, self.config.vsync());

        self.renderer
            .on_new_swapchain(swapchain_properties, &self.simple_render_pass);

        let swapchain = Swapchain::create(
            Arc::clone(&self.context),
            swapchain_support_details,
            dimensions,
            self.config.vsync(),
            &self.simple_render_pass,
        );

        let command_buffers =
            Self::allocate_command_buffers(&self.context, swapchain.image_count());

        self.swapchain = swapchain;
        self.swapchain_properties = swapchain_properties;
        self.command_buffers = command_buffers;
    }

    fn has_window_been_minimized(&self) -> bool {
        match self.window.get_inner_size() {
            Some(LogicalSize { width, height }) if width == 0.0 || height == 0.0 => true,
            _ => false,
        }
    }

    fn has_window_been_maximized(&self) -> bool {
        match self.window.get_inner_size() {
            Some(LogicalSize { width, height }) if width > 0.0 && height > 0.0 => true,
            _ => false,
        }
    }

    /// Clean up the swapchain and all resources that depends on it.
    fn cleanup_swapchain(&mut self) {
        let device = self.context.device();
        unsafe {
            device.free_command_buffers(self.context.general_command_pool(), &self.command_buffers);
        }
        self.swapchain.destroy();
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        log::debug!("Dropping application.");
        self.cleanup_swapchain();
        let device = self.context.device();
        self.in_flight_frames.destroy(device);
    }
}

#[derive(Clone, Copy)]
struct SyncObjects {
    image_available_semaphore: vk::Semaphore,
    render_finished_semaphore: vk::Semaphore,
    fence: vk::Fence,
}

impl SyncObjects {
    fn destroy(&self, device: &Device) {
        unsafe {
            device.destroy_semaphore(self.image_available_semaphore, None);
            device.destroy_semaphore(self.render_finished_semaphore, None);
            device.destroy_fence(self.fence, None);
        }
    }
}

struct InFlightFrames {
    sync_objects: Vec<SyncObjects>,
    current_frame: usize,
}

impl InFlightFrames {
    fn new(sync_objects: Vec<SyncObjects>) -> Self {
        Self {
            sync_objects,
            current_frame: 0,
        }
    }

    fn destroy(&self, device: &Device) {
        self.sync_objects.iter().for_each(|o| o.destroy(&device));
    }
}

impl Iterator for InFlightFrames {
    type Item = SyncObjects;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.sync_objects[self.current_frame];

        self.current_frame = (self.current_frame + 1) % self.sync_objects.len();

        Some(next)
    }
}
