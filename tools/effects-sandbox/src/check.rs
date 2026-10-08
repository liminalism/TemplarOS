//! The shader gate: every module parses and validates under naga, and every entry point
//! translates to GLSL ES 3.0 (the compositor's GLES renderer) and to SPIR-V (Vulkan).

use naga::back::{glsl, spv};
use naga::valid::{Capabilities, ValidationFlags, Validator};

/// Validate and translate everything. Returns a report and whether all of it passed.
pub fn run(verbose_dir: Option<&std::path::Path>) -> (String, bool) {
    let mut report = String::new();
    let mut ok = true;
    let sandbox = format!(
        "{}{}",
        templar_effects::shaders::COMMON,
        crate::gpu::SANDBOX_WGSL
    );
    let mut modules: Vec<(&str, String, Vec<&str>, Vec<&str>)> = templar_effects::shaders::ALL
        .iter()
        .map(|m| {
            (
                m.name,
                m.source.to_owned(),
                m.vertex.to_vec(),
                m.fragment.to_vec(),
            )
        })
        .collect();
    modules.push(("sandbox", sandbox, vec!["vs_quad"], vec!["fs_quad"]));

    for (name, source, vertex, fragment) in &modules {
        let module = match naga::front::wgsl::parse_str(source) {
            Ok(module) => module,
            Err(error) => {
                ok = false;
                report.push_str(&format!(
                    "FAIL {name}: parse\n{}\n",
                    error.emit_to_string(source)
                ));
                continue;
            }
        };
        let info =
            match Validator::new(ValidationFlags::all(), Capabilities::empty()).validate(&module) {
                Ok(info) => info,
                Err(error) => {
                    ok = false;
                    report.push_str(&format!(
                        "FAIL {name}: validate\n{}\n",
                        error.emit_to_string(source)
                    ));
                    continue;
                }
            };
        let stages = vertex
            .iter()
            .map(|e| (*e, naga::ShaderStage::Vertex))
            .chain(fragment.iter().map(|e| (*e, naga::ShaderStage::Fragment)));
        for (entry, stage) in stages {
            let options = glsl::Options {
                version: glsl::Version::Embedded {
                    version: 300,
                    is_webgl: false,
                },
                ..glsl::Options::default()
            };
            let pipeline = glsl::PipelineOptions {
                shader_stage: stage,
                entry_point: (*entry).to_owned(),
                multiview: None,
            };
            let mut out = String::new();
            let glsl_result = glsl::Writer::new(
                &mut out,
                &module,
                &info,
                &options,
                &pipeline,
                naga::proc::BoundsCheckPolicies::default(),
            )
            .and_then(|mut writer| writer.write().map(|_| ()));
            let spv_result = spv::write_vec(
                &module,
                &info,
                &spv::Options::default(),
                Some(&spv::PipelineOptions {
                    shader_stage: stage,
                    entry_point: (*entry).to_owned(),
                }),
            );
            match (&glsl_result, &spv_result) {
                (Ok(()), Ok(words)) => {
                    report.push_str(&format!(
                        "ok   {name}::{entry:<11} GLSL ES 300 {:>6} B   SPIR-V {:>6} B\n",
                        out.len(),
                        words.len() * 4
                    ));
                    if let Some(dir) = verbose_dir {
                        let _ = std::fs::create_dir_all(dir);
                        let _ =
                            std::fs::write(dir.join(format!("{name}.{entry}.es300.glsl")), &out);
                    }
                }
                _ => {
                    ok = false;
                    report.push_str(&format!(
                        "FAIL {name}::{entry}: glsl {glsl_result:?} spv {:?}\n",
                        spv_result.err()
                    ));
                }
            }
        }
    }
    (report, ok)
}
