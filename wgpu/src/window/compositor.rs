use crate::{Backend, Color, Error, Renderer, Settings, Viewport};

use futures::task::SpawnExt;
use iced_native::{futures, mouse};
use raw_window_handle::HasRawWindowHandle;

/// A window graphics backend for iced powered by `wgpu`.
#[allow(missing_debug_implementations)]
pub struct Compositor {
    settings: Settings,
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    staging_belt: wgpu::util::StagingBelt,
    local_pool: futures::executor::LocalPool,
}

#[derive(Clone)]
pub enum WgpuBackend {
    Auto,
    Vulkan,
    Metal,
    Dx12,
    Dx11,
    Gl,
    BrowserWgpu,
}

impl WgpuBackend {
    fn from_env() -> Self {
        if let Ok(backend) = std::env::var("WGPU_BACKEND") {
            match backend.to_lowercase().as_str() {
                "vulkan" => WgpuBackend::Vulkan,
                "metal" => WgpuBackend::Metal,
                "dx12" => WgpuBackend::Dx12,
                "dx11" => WgpuBackend::Dx11,
                "gl" => WgpuBackend::Gl,
                "webgpu" => WgpuBackend::BrowserWgpu,
                other => panic!("Unknown backend: {}", other),
            }
        } else {
            WgpuBackend::Auto
        }
    }
}

impl Compositor {
    const CHUNK_SIZE: u64 = 10 * 1024;

    /// Requests a new [`Compositor`] with the given [`Settings`].
    ///
    /// Returns `None` if no compatible graphics adapter could be found.
    pub async fn request(settings: Settings) -> Option<Self> {
        let backend = match WgpuBackend::from_env() {
            WgpuBackend::Auto => wgpu::BackendBit::PRIMARY,
            WgpuBackend::Vulkan => wgpu::BackendBit::VULKAN,
            WgpuBackend::Metal => wgpu::BackendBit::METAL,
            WgpuBackend::Dx12 => wgpu::BackendBit::DX12,
            WgpuBackend::Dx11 => wgpu::BackendBit::DX11,
            WgpuBackend::Gl => wgpu::BackendBit::GL,
            WgpuBackend::BrowserWgpu => wgpu::BackendBit::BROWSER_WEBGPU,
        };

        let instance = wgpu::Instance::new(backend);

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: if settings.antialiasing.is_none() {
                    wgpu::PowerPreference::LowPower
                } else {
                    wgpu::PowerPreference::HighPerformance
                },
                compatible_surface: None,
            })
            .await?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some(
                        "iced_wgpu::window::compositor device descriptor",
                    ),
                    features: wgpu::Features::empty(),
                    limits: wgpu::Limits {
                        max_bind_groups: 2,
                        ..wgpu::Limits::default()
                    },
                },
                None,
            )
            .await
            .ok()?;

        let staging_belt = wgpu::util::StagingBelt::new(Self::CHUNK_SIZE);
        let local_pool = futures::executor::LocalPool::new();

        Some(Compositor {
            instance,
            settings,
            device,
            queue,
            staging_belt,
            local_pool,
        })
    }

    /// Creates a new rendering [`Backend`] for this [`Compositor`].
    pub fn create_backend(&self) -> Backend {
        Backend::new(&self.device, self.settings)
    }
}

impl iced_graphics::window::Compositor for Compositor {
    type Settings = Settings;
    type Renderer = Renderer;
    type Surface = wgpu::Surface;
    type SwapChain = wgpu::SwapChain;

    fn new(settings: Self::Settings) -> Result<(Self, Renderer), Error> {
        let compositor = futures::executor::block_on(Self::request(settings))
            .ok_or(Error::AdapterNotFound)?;

        let backend = compositor.create_backend();

        Ok((compositor, Renderer::new(backend)))
    }

    fn create_surface<W: HasRawWindowHandle>(
        &mut self,
        window: &W,
    ) -> wgpu::Surface {
        #[allow(unsafe_code)]
        unsafe {
            self.instance.create_surface(window)
        }
    }

    fn create_swap_chain(
        &mut self,
        surface: &Self::Surface,
        width: u32,
        height: u32,
    ) -> Self::SwapChain {
        self.device.create_swap_chain(
            surface,
            &wgpu::SwapChainDescriptor {
                usage: wgpu::TextureUsage::RENDER_ATTACHMENT,
                format: self.settings.format,
                present_mode: self.settings.present_mode,
                width,
                height,
            },
        )
    }

    fn draw<T: AsRef<str>>(
        &mut self,
        renderer: &mut Self::Renderer,
        swap_chain: &mut Self::SwapChain,
        viewport: &Viewport,
        background_color: Color,
        output: &<Self::Renderer as iced_native::Renderer>::Output,
        overlay: &[T],
    ) -> mouse::Interaction {
        let frame = swap_chain.get_current_frame().expect("Next frame");

        let mut encoder = self.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor {
                label: Some("iced_wgpu encoder"),
            },
        );

        let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("iced_wgpu::window::Compositor render pass"),
            color_attachments: &[wgpu::RenderPassColorAttachmentDescriptor {
                attachment: &frame.output.view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear({
                        let [r, g, b, a] = background_color.into_linear();

                        wgpu::Color {
                            r: f64::from(r),
                            g: f64::from(g),
                            b: f64::from(b),
                            a: f64::from(a),
                        }
                    }),
                    store: true,
                },
            }],
            depth_stencil_attachment: None,
        });

        let mouse_interaction = renderer.backend_mut().draw(
            &mut self.device,
            &mut self.staging_belt,
            &mut encoder,
            &frame.output.view,
            viewport,
            output,
            overlay,
        );

        // Submit work
        self.staging_belt.finish();
        self.queue.submit(Some(encoder.finish()));

        // Recall staging buffers
        self.local_pool
            .spawner()
            .spawn(self.staging_belt.recall())
            .expect("Recall staging belt");

        self.local_pool.run_until_stalled();

        mouse_interaction
    }
}
