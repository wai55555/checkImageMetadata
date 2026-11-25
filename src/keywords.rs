#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PngKeyword {
    pub name: &'static str,
    pub label: &'static str,
    pub json_settings: bool,
    pub xmp_packet: bool,
}

static PNG_KEYWORDS: [PngKeyword; 14] = [
    PngKeyword {
        name: "parameters",
        label: "Stable Diffusion (A1111)",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "prompt",
        label: "prompt",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "workflow",
        label: "workflow",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "generation_data",
        label: "generation_data",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "Description",
        label: "NovelAI [Prompt]",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "Comment",
        label: "NovelAI [Settings]",
        json_settings: true,
        xmp_packet: false,
    },
    PngKeyword {
        name: "Title",
        label: "Title",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "Software",
        label: "Software",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "Source",
        label: "Source",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "sui_image_params",
        label: "SwarmUI [Params]",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "sui_extra_data",
        label: "SwarmUI [Extra]",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "invokeai_metadata",
        label: "InvokeAI",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "sd-metadata",
        label: "sd-metadata",
        json_settings: false,
        xmp_packet: false,
    },
    PngKeyword {
        name: "XML:com.adobe.xmp",
        label: "XMP",
        json_settings: false,
        xmp_packet: true,
    },
];

pub fn registered() -> impl Iterator<Item = &'static [u8]> {
    PNG_KEYWORDS.iter().map(|entry| entry.name.as_bytes())
}

pub fn lookup(keyword: &[u8]) -> Option<PngKeyword> {
    PNG_KEYWORDS
        .iter()
        .find(|entry| entry.name.as_bytes() == keyword)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::{lookup, registered, PNG_KEYWORDS};

    #[test]
    fn registry_is_the_single_source_for_scanning_and_lookup() {
        let registered: Vec<_> = registered().collect();
        let table: Vec<_> = PNG_KEYWORDS
            .iter()
            .map(|entry| entry.name.as_bytes())
            .collect();
        assert_eq!(registered, table);
        assert_eq!(registered.len(), 14);

        for (index, entry) in PNG_KEYWORDS.iter().enumerate() {
            assert_eq!(lookup(entry.name.as_bytes()), Some(*entry));
            assert!(!PNG_KEYWORDS[index + 1..]
                .iter()
                .any(|next| next.name == entry.name));
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
