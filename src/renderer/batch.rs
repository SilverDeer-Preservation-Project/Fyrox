//! The module responsible for batch generation for rendering optimizations.

use crate::scene::mesh::Mesh;
use crate::scene::terrain::Terrain;
use crate::{
    core::{
        algebra::Matrix4, arrayvec::ArrayVec, parking_lot::Mutex, pool::Handle, scope_profile,
        sstorage::ImmutableString,
    },
    material::{Material, PropertyValue},
    scene::{
        graph::Graph,
        mesh::{surface::SurfaceData, RenderPath},
        node::Node,
    },
    utils::log::{Log, MessageKind},
};
use fxhash::{FxHashMap, FxHasher};
use std::{
    fmt::{Debug, Formatter},
    hash::Hasher,
    sync::Arc,
};

/// Maximum amount of bone matrices per instance.
pub const BONE_MATRICES_COUNT: usize = 64;

/// A set of data of a surface for rendering.  
pub struct SurfaceInstance {
    /// A handle to an owner node.
    pub owner: Handle<Node>,
    /// A world matrix.
    pub world_transform: Matrix4<f32>,
    /// A set of bone matrices.
    pub bone_matrices: ArrayVec<Matrix4<f32>, BONE_MATRICES_COUNT>,
    /// A depth-hack value.
    pub depth_offset: f32,
}

/// A set of surface instances that share the same vertex/index data and a material.
pub struct Batch {
    /// A pointer to shared surface data.
    pub data: Arc<Mutex<SurfaceData>>,
    /// A set of instances.
    pub instances: Vec<SurfaceInstance>,
    /// A material that is shared across all instances.
    pub material: Arc<Mutex<Material>>,
    /// Whether the batch is using GPU skinning or not.
    pub is_skinned: bool,
    /// A render path of the batch.
    pub render_path: RenderPath,
    /// A decal layer index of the batch.
    pub decal_layer_index: u8,
    sort_index: u64,
}

impl Debug for Batch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Batch {}: {} instances",
            &*self.data as *const _ as u64,
            self.instances.len()
        )
    }
}

/// Batch storage handles batch generation for a scene before rendering. It is used to optimize
/// rendering by reducing amount of state changes of OpenGL context.
#[derive(Default)]
pub struct BatchStorage {
    buffers: Vec<Vec<SurfaceInstance>>,
    batch_map: FxHashMap<u64, usize>,
    /// Sorted list of batches.
    pub batches: Vec<Batch>,
}

impl BatchStorage {
    pub(in crate) fn generate_batches(&mut self, graph: &Graph) {
        scope_profile!();

        for batch in self.batches.iter_mut() {
            batch.instances.clear();
            self.buffers.push(std::mem::take(&mut batch.instances));
        }

        self.batches.clear();
        self.batch_map.clear();

        for (handle, node) in graph.pair_iter() {
            if let Some(mesh) = node.cast::<Mesh>() {
                for surface in mesh.surfaces().iter() {
                    let is_skinned = !surface.bones.is_empty();

                    let world = if is_skinned {
                        Matrix4::identity()
                    } else {
                        mesh.global_transform()
                    };

                    let data = surface.data();
                    let batch_id = surface.batch_id();

                    let batch = if let Some(&batch_index) = self.batch_map.get(&batch_id) {
                        self.batches.get_mut(batch_index).unwrap()
                    } else {
                        self.batch_map.insert(batch_id, self.batches.len());
                        self.batches.push(Batch {
                            data,
                            // Batches from meshes will be sorted using materials.
                            // This will significantly reduce pipeline state changes.
                            sort_index: surface.material_id(),
                            instances: self.buffers.pop().unwrap_or_default(),
                            material: surface.material().clone(),
                            is_skinned: !surface.bones.is_empty(),
                            render_path: mesh.render_path(),
                            decal_layer_index: mesh.decal_layer_index(),
                        });
                        self.batches.last_mut().unwrap()
                    };

                    batch.sort_index = surface.material_id();
                    batch.material = surface.material().clone();

                    batch.instances.push(SurfaceInstance {
                        world_transform: world,
                        bone_matrices: surface
                            .bones
                            .iter()
                            .map(|&bone_handle| {
                                let bone_node = &graph[bone_handle];
                                bone_node.global_transform() * bone_node.inv_bind_pose_transform()
                            })
                            .collect(),
                        owner: handle,
                        depth_offset: mesh.depth_offset_factor(),
                    });
                }
            } else if let Some(terrain) = node.cast::<Terrain>() {
                for (layer_index, layer) in terrain.layers().iter().enumerate() {
                    for (chunk_index, chunk) in terrain.chunks_ref().iter().enumerate() {
                        let data = chunk.data();
                        let data_key = &*data as *const _ as u64;

                        let mut material = (*layer.material.lock()).clone();
                        match material.set_property(
                            &ImmutableString::new(&layer.mask_property_name),
                            PropertyValue::Sampler {
                                value: Some(layer.chunk_masks[chunk_index].clone()),
                                fallback: Default::default(),
                            },
                        ) {
                            Ok(_) => {
                                let material = Arc::new(Mutex::new(material));

                                let mut hasher = FxHasher::default();

                                hasher.write_u64(&*material as *const _ as u64);
                                hasher.write_u64(data_key);

                                let key = hasher.finish();

                                let batch = if let Some(&batch_index) = self.batch_map.get(&key) {
                                    self.batches.get_mut(batch_index).unwrap()
                                } else {
                                    self.batch_map.insert(key, self.batches.len());
                                    self.batches.push(Batch {
                                        data: data.clone(),
                                        instances: self.buffers.pop().unwrap_or_default(),
                                        material: material.clone(),
                                        is_skinned: false,
                                        render_path: RenderPath::Deferred,
                                        sort_index: layer_index as u64,
                                        decal_layer_index: terrain.decal_layer_index(),
                                    });
                                    self.batches.last_mut().unwrap()
                                };

                                batch.sort_index = layer_index as u64;
                                batch.material = material;

                                batch.instances.push(SurfaceInstance {
                                    world_transform: terrain.global_transform(),
                                    bone_matrices: Default::default(),
                                    owner: handle,
                                    depth_offset: terrain.depth_offset_factor(),
                                });
                            }
                            Err(e) => Log::writeln(
                                MessageKind::Error,
                                format!(
                                    "Failed to prepare batch for terrain chunk.\
                                 Unable to set mask texture for terrain material. Reason: {:?}",
                                    e
                                ),
                            ),
                        }
                    }
                }
            }
        }

        for batch in self.batches.iter_mut() {
            batch.instances.shrink_to_fit();
        }

        self.batches.sort_unstable_by_key(|b| b.sort_index);
    }
}
