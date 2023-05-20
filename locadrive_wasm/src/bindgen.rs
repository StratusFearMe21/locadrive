use sledgehammer_bindgen::bindgen;

#[bindgen]
mod js {

    struct Channel;

    const JS: &str = r#"const existing_dir_select=document.getElementById("exist_dir_select");const new_dir_node=document.getElementById("new_directory");const transfer_buttons=document.getElementById("complete_transfer_buttons");const transfer_uncompressed=document.getElementById("transfer_as_tar");const transfer_zst=document.getElementById("transfer_as_tzst");const transfer_gz=document.getElementById("transfer_as_tgz");"#;

    fn add_path(value: &str) {
        r#"{const option=document.createElement("option");option.setAttribute("value",$value$);option.textContent=$value$;existing_dir_select.appendChild(option);}existing_dir_select.value=$value$;new_dir_node.value="";"#
    }

    fn unhide_transfer_buttons() {
        r#"transfer_buttons.removeAttribute("style");"#
    }

    fn hide_transfer_buttons() {
        r#"transfer_buttons.setAttribute("style","visibility: hidden");"#
    }

    fn set_transfer_buttons(
        uncompressed: impl Writable<u8>,
        zst: impl Writable<u8>,
        gz: impl Writable<u8>,
    ) {
        r#"transfer_uncompressed.setAttribute("href",$uncompressed$);transfer_zst.setAttribute("href",$zst$);transfer_gz.setAttribute("href",$gz$);"#
    }

    fn clear_dir_select() {
        r#"existing_dir_select.innerHtml="";"#
    }

    fn append_path(path: &str) {
        r#"const option=document.createElement("option");option.setAttribute("value",$path$);option.textContent=$path$;existing_dir_select.appendChild(option);"#
    }
}
