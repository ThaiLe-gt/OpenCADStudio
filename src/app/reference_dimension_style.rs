use codec::{tables::DimStyle, CadDocument};

/// Keep the effective style, including per-entity overrides. Resource handles
/// remain bound to the source document. A missing text resource uses its
/// native Standard style; the read path reports that substitution for review.
pub(crate) fn copy_style(
    document: &CadDocument,
    reference: &DimStyle,
    name: &str,
) -> Result<DimStyle, String> {
    let source = document.dim_styles.get(&reference.name).ok_or_else(|| {
        format!(
            "Reference dimension style {} is missing from this drawing; import its resources first",
            reference.name
        )
    })?;
    if source.dimtxsty_handle != reference.dimtxsty_handle
        || source.dimblk != reference.dimblk
        || source.dimblk1 != reference.dimblk1
        || source.dimblk2 != reference.dimblk2
    {
        return Err("Reference dimension style resources do not belong to this drawing".into());
    }
    let mut style = reference.clone();
    // Imported resource handles may collide with unrelated objects even when
    // the effective and table DIMSTYLE agree. Bind the named text resource in
    // this document instead of propagating that stale pointer into new dims.
    let text_style = document
        .text_styles
        .get(&reference.dimtxsty)
        .or_else(|| {
            // Renamed/imported styles can retain an old DIMTXSTY name. Only
            // accept a handle belonging to a text style in this document.
            document.text_styles.iter().find(|entry| {
                !reference.dimtxsty_handle.is_null()
                    && entry.handle == reference.dimtxsty_handle
            })
        })
        .or_else(|| document.text_styles.get("Standard"))
        .ok_or_else(|| {
            format!(
                "Reference text style {} is missing from this drawing",
                reference.dimtxsty
            )
        })?;
    style.dimtxsty_handle = text_style.handle;
    style.dimtxsty = text_style.name.clone();
    style.name = name.to_owned();
    style.handle = Default::default();
    Ok(style)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_stale_text_name_by_native_handle() {
        let mut document = CadDocument::new();
        let mut source = DimStyle::new("Imported");
        source.dimtxsty = "Gdim".into();
        source.dimtxsty_handle = document.text_styles.get("Standard").unwrap().handle;
        document.dim_styles.add(source.clone()).unwrap();
        let copy = copy_style(&document, &source, "Copy").unwrap();
        assert_eq!(copy.dimtxsty, "Standard");
        assert_eq!(copy.dimtxsty_handle, source.dimtxsty_handle);
    }
    #[test]
    fn substitutes_standard_for_missing_text_but_rejects_foreign_resource_handles() {
        let mut document = CadDocument::new();
        let mut source = DimStyle::new("Imported");
        source.dimtxsty = "Gdim".into();
        source.dimtxsty_handle = document.layers.iter().next().unwrap().handle;
        document.dim_styles.add(source.clone()).unwrap();
        let fallback = copy_style(&document, &source, "Copy").unwrap();
        assert_eq!(fallback.dimtxsty, "Standard");
        assert_eq!(fallback.dimtxsty_handle, document.text_styles.get("Standard").unwrap().handle);
        source.dimtxsty_handle = document.text_styles.get("Standard").unwrap().handle;
        assert!(copy_style(&document, &source, "Copy").unwrap_err().contains("do not belong"));
    }
    #[test]
    fn rebinds_imported_text_pointer_to_the_named_text_style() {
        let mut document = CadDocument::new();
        let mut source = DimStyle::new("Imported");
        source.dimtxsty_handle = document.layers.iter().next().unwrap().handle;
        document.dim_styles.add(source.clone()).unwrap();
        let expected = document.text_styles.get(&source.dimtxsty).unwrap().handle;
        assert_ne!(source.dimtxsty_handle, expected);
        assert_eq!(
            copy_style(&document, &source, "Copy")
                .unwrap()
                .dimtxsty_handle,
            expected
        );
    }
    #[test]
    fn preserves_effective_style_instead_of_active_defaults() {
        let mut document = CadDocument::new();
        let mut source = DimStyle::new("Gdim50");
        source.dimtxt = 3.0;
        source.dimasz = 6.0;
        source.dimscale = 15.0;
        source.dimtad = 1;
        source.dimtih = false;
        source.dimtoh = false;
        source.dimdec = 0;
        source.dimclrd = 1;
        source.dimclre = 1;
        source.dimclrt = 256;
        document.dim_styles.add(source.clone()).unwrap();
        // An effective entity override must also survive.
        source.dimgap = 1.25;
        let actual = copy_style(&document, &source, "NEW").unwrap();
        source.name = "NEW".into();
        source.handle = Default::default();
        source.dimtxsty_handle = document.text_styles.get(&source.dimtxsty).unwrap().handle;
        assert_eq!(actual, source);
        source.name = "MISSING".into();
        assert!(copy_style(&document, &source, "NEW").is_err());
    }
}
