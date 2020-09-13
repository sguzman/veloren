use super::super::{AaMode, GlobalsLayouts, Renderer, Texture};
use vek::*;
use zerocopy::AsBytes;

#[repr(C)]
#[derive(Copy, Clone, Debug, AsBytes)]
pub struct Vertex {
    pos: [f32; 2],
}

impl Vertex {
    pub fn new(pos: Vec2<f32>) -> Self {
        Self {
            pos: pos.into_array(),
        }
    }

    fn desc<'a>() -> wgpu::VertexBufferDescriptor<'a> {
        use std::mem;
        wgpu::VertexBufferDescriptor {
            stride: mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::InputStepMode::Vertex,
            attributes: &[wgpu::VertexAttributeDescriptor {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float2,
            }],
        }
    }
}

pub struct LodData {
    pub map: Texture,
    pub alt: Texture,
    pub horizon: Texture,
    pub tgt_detail: u32,
}

impl LodData {
    pub fn new(
        renderer: &mut Renderer,
        map_size: Vec2<u16>,
        lod_base: &[u32],
        lod_alt: &[u32],
        lod_horizon: &[u32],
        tgt_detail: u32,
        //border_color: gfx::texture::PackedColor,
    ) -> Self {
        let mut texture_info = wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: map_size.x,
                height: map_size.y,
                depth: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsage::SAMPLED | wgpu::TextureUsage::COPY_DST,
        };

        let sampler_info = wgpu::SamplerDescriptor {
            label: None,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            border_color: Some(wgpu::SamplerBorderColor::TransparentBlack),
            ..Default::default()
        };

        let map = renderer.create_texture_with_data_raw(
            &texture_info,
            &sampler_info,
            map_size.x * 4,
            [map_size.x, map_size.y],
            lod_base.as_bytes(),
        );
        texture_info = wgpu::TextureFormat::Rg16Uint;
        let alt = renderer.create_texture_with_data_raw(
            &texture_info,
            &sampler_info,
            map_size.x * 4,
            [map_size.x, map_size.y],
            lod_base.as_bytes(),
        );
        texture_info = wgpu::TextureFormat::Rgba8Unorm;
        let horizon = renderer.create_texture_with_data_raw(
            &texture_info,
            &sampler_info,
            map_size.x * 4,
            [map_size.x, map_size.y],
            lod_base.as_bytes(),
        );

        Self {
            map,
            alt,
            horizon,
            tgt_detail,
        }

        // Self {
        //     map: renderer
        //         .create_texture_immutable_raw(
        //             kind,
        //             gfx::texture::Mipmap::Provided,
        //             &[gfx::memory::cast_slice(lod_base)],
        //             SamplerInfo {
        //                 border: border_color,
        //                 ..info
        //             },
        //         )
        //         .expect("Failed to generate map texture"),
        //     alt: renderer
        //         .create_texture_immutable_raw(
        //             kind,
        //             gfx::texture::Mipmap::Provided,
        //             &[gfx::memory::cast_slice(lod_alt)],
        //             SamplerInfo {
        //                 border: [0.0, 0.0, 0.0, 0.0].into(),
        //                 ..info
        //             },
        //         )
        //         .expect("Failed to generate alt texture"),
        //     horizon: renderer
        //         .create_texture_immutable_raw(
        //             kind,
        //             gfx::texture::Mipmap::Provided,
        //             &[gfx::memory::cast_slice(lod_horizon)],
        //             SamplerInfo {
        //                 border: [1.0, 0.0, 1.0, 0.0].into(),
        //                 ..info
        //             },
        //         )
        //         .expect("Failed to generate horizon texture"),
        //     tgt_detail,
        // }
    }
}

pub struct LodTerrainPipeline {
    pub pipeline: wgpu::RenderPipeline,
}

impl LodTerrainPipeline {
    pub fn new(
        device: &wgpu::Device,
        vs_module: &wgpu::ShaderModule,
        fs_module: &wgpu::ShaderModule,
        sc_desc: &wgpu::SwapChainDescriptor,
        global_layout: &GlobalsLayouts,
        aa_mode: AaMode,
    ) -> Self {
        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Lod terrain pipeline layout"),
                push_constant_ranges: &[],
                bind_group_layouts: &[&global_layout.globals],
            });

        let samples = match aa_mode {
            AaMode::None | AaMode::Fxaa => 1,
            // TODO: Ensure sampling in the shader is exactly between the 4 texels
            AaMode::SsaaX4 => 1,
            AaMode::MsaaX4 => 4,
            AaMode::MsaaX8 => 8,
            AaMode::MsaaX16 => 16,
        };

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Lod terrain pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex_stage: wgpu::ProgrammableStageDescriptor {
                module: vs_module,
                entry_point: "main",
            },
            fragment_stage: Some(wgpu::ProgrammableStageDescriptor {
                module: fs_module,
                entry_point: "main",
            }),
            rasterization_state: Some(wgpu::RasterizationStateDescriptor {
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: wgpu::CullMode::Back,
                clamp_depth: false,
                depth_bias: 0,
                depth_bias_slope_scale: 0.0,
                depth_bias_clamp: 0.0,
            }),
            primitive_topology: wgpu::PrimitiveTopology::TriangleList,
            color_states: &[wgpu::ColorStateDescriptor {
                format: sc_desc.format,
                color_blend: wgpu::BlendDescriptor::REPLACE,
                alpha_blend: wgpu::BlendDescriptor::REPLACE,
                write_mask: wgpu::ColorWrite::ALL,
            }],
            depth_stencil_state: Some(wgpu::DepthStencilStateDescriptor {
                format: wgpu::TextureFormat::Depth24Plus,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::LessEqual,
                stencil: wgpu::StencilStateDescriptor {
                    front: wgpu::StencilStateFaceDescriptor::IGNORE,
                    back: wgpu::StencilStateFaceDescriptor::IGNORE,
                    read_mask: !0,
                    write_mask: !0,
                },
            }),
            vertex_state: wgpu::VertexStateDescriptor {
                index_format: wgpu::IndexFormat::Uint16,
                vertex_buffers: &[Vertex::desc()],
            },
            sample_count: samples,
            sample_mask: !0,
            alpha_to_coverage_enabled: false,
        });

        Self {
            pipeline: render_pipeline,
        }
    }
}
