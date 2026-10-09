use anyhow::{Result, ensure};
use templar_types::wallpaper::{TextureLimit, WallpaperConfig};

pub fn validate(config: &WallpaperConfig) -> Result<()> {
    ensure!(
        config.sources.len() <= 256,
        "at most 256 source roots are supported"
    );
    for source in &config.sources {
        validate_path(source)?;
    }
    if let Some(seconds) = config.interval_seconds {
        ensure!(
            (300..=3600).contains(&seconds),
            "rotation interval must be 300..=3600 seconds"
        );
    }
    ensure!(config.ground[3] == 255, "wallpaper ground must be opaque");
    validate_texture(config.texture)?;
    ensure!(
        (16 * 1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&config.worker_budget_bytes),
        "worker budget must be 16 MiB..=2 GiB"
    );
    ensure!(
        config.transition_budget_bytes >= 1024 * 1024
            && config.transition_budget_bytes <= 4 * 1024 * 1024 * 1024,
        "transition budget must be 1 MiB..=4 GiB"
    );
    if let Some(colors) = config.quantize_colors {
        ensure!(
            (1..=256).contains(&colors),
            "quantize colors must be 1..=256"
        );
    }
    ensure!(
        config.outputs.len() <= 64,
        "at most 64 output overrides are supported"
    );
    for (output, value) in &config.outputs {
        ensure!(
            !output.is_empty() && output.len() <= 256,
            "invalid output identifier"
        );
        if let Some(ground) = value.ground {
            ensure!(ground[3] == 255, "output ground must be opaque");
        }
        if let Some(texture) = value.texture {
            validate_texture(texture)?;
        }
        if let Some(source) = &value.source {
            validate_path(source)?;
        }
    }
    Ok(())
}
fn validate_path(path: &[u8]) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 4096 && !path.contains(&0),
        "source path must contain 1..=4096 non-NUL bytes"
    );
    Ok(())
}
pub fn validate_texture(texture: TextureLimit) -> Result<()> {
    match texture {
        TextureLimit::Native => (),
        TextureLimit::MaxEdge { pixels } => {
            ensure!(pixels > 0, "texture edge ceiling must be positive")
        }
        TextureLimit::MaxPixels { pixels } => {
            ensure!(pixels > 0, "texture pixel ceiling must be positive")
        }
    }
    Ok(())
}
pub fn effective(config: &WallpaperConfig, output: &str) -> WallpaperConfig {
    let mut result = config.clone();
    if let Some(value) = config.outputs.get(output) {
        if let Some(sizing) = value.sizing {
            result.sizing = sizing;
        }
        if let Some(ground) = value.ground {
            result.ground = ground;
        }
        if let Some(texture) = value.texture {
            result.texture = texture;
        }
        if let Some(source) = &value.source {
            result.sources = vec![source.clone()];
            result.recursive = false;
        }
    }
    result.outputs.clear();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_translucent_ground_and_zero_ceiling() {
        let mut config = WallpaperConfig::default();
        config.ground[3] = 254;
        assert!(validate(&config).is_err());
        config.ground[3] = 255;
        config.texture = TextureLimit::MaxPixels { pixels: 0 };
        assert!(validate(&config).is_err());
    }
}
