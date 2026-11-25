#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PngKeyword {
    pub name: &'static str,
    pub label: &'static str,
    pub json_settings: bool,
    pub xmp_packet: bool,
}

pub fn lookup(keyword: &[u8]) -> Option<PngKeyword> {
    let entry = match keyword {
        b"parameters" => PngKeyword {
            name: "parameters",
            label: "Stable Diffusion (A1111)",
            json_settings: false,
            xmp_packet: false,
        },
        b"prompt" => PngKeyword {
            name: "prompt",
            label: "prompt",
            json_settings: false,
            xmp_packet: false,
        },
        b"workflow" => PngKeyword {
            name: "workflow",
            label: "workflow",
            json_settings: false,
            xmp_packet: false,
        },
        b"generation_data" => PngKeyword {
            name: "generation_data",
            label: "generation_data",
            json_settings: false,
            xmp_packet: false,
        },
        b"Description" => PngKeyword {
            name: "Description",
            label: "NovelAI [Prompt]",
            json_settings: false,
            xmp_packet: false,
        },
        b"Comment" => PngKeyword {
            name: "Comment",
            label: "NovelAI [Settings]",
            json_settings: true,
            xmp_packet: false,
        },
        b"Title" => PngKeyword {
            name: "Title",
            label: "Title",
            json_settings: false,
            xmp_packet: false,
        },
        b"Software" => PngKeyword {
            name: "Software",
            label: "Software",
            json_settings: false,
            xmp_packet: false,
        },
        b"Source" => PngKeyword {
            name: "Source",
            label: "Source",
            json_settings: false,
            xmp_packet: false,
        },
        b"sui_image_params" => PngKeyword {
            name: "sui_image_params",
            label: "SwarmUI [Params]",
            json_settings: false,
            xmp_packet: false,
        },
        b"sui_extra_data" => PngKeyword {
            name: "sui_extra_data",
            label: "SwarmUI [Extra]",
            json_settings: false,
            xmp_packet: false,
        },
        b"invokeai_metadata" => PngKeyword {
            name: "invokeai_metadata",
            label: "InvokeAI",
            json_settings: false,
            xmp_packet: false,
        },
        b"sd-metadata" => PngKeyword {
            name: "sd-metadata",
            label: "sd-metadata",
            json_settings: false,
            xmp_packet: false,
        },
        b"XML:com.adobe.xmp" => PngKeyword {
            name: "XML:com.adobe.xmp",
            label: "XMP",
            json_settings: false,
            xmp_packet: true,
        },
        _ => return None,
    };
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::lookup;

    #[test]
    fn registry_contains_only_the_specified_png_keywords() {
        let keys: [&[u8]; 14] = [
            b"parameters",
            b"prompt",
            b"workflow",
            b"generation_data",
            b"Description",
            b"Comment",
            b"Title",
            b"Software",
            b"Source",
            b"sui_image_params",
            b"sui_extra_data",
            b"invokeai_metadata",
            b"sd-metadata",
            b"XML:com.adobe.xmp",
        ];
        for key in keys {
            assert!(lookup(key).is_some(), "missing keyword: {key:?}");
        }
        assert!(lookup(b"Author").is_none());
        assert!(lookup(b"PARAMETERS").is_none());
    }

    #[test]
    fn swarm_and_invoke_keywords_have_the_specified_labels() {
        assert_eq!(
            lookup(b"sui_image_params").unwrap().label,
            "SwarmUI [Params]"
        );
        assert_eq!(lookup(b"sui_extra_data").unwrap().label, "SwarmUI [Extra]");
        assert_eq!(lookup(b"invokeai_metadata").unwrap().label, "InvokeAI");
        assert_eq!(lookup(b"sd-metadata").unwrap().label, "sd-metadata");
    }
}
