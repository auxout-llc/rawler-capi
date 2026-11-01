// SPDX-License-Identifier: LGPL-2.1
// Give the shared library a relocatable name so that dependents find it next to
// themselves (Apple: @rpath, ELF: SONAME).
fn main() {
  let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  match os.as_str() {
    "macos" | "ios" => {
      println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/librawler_capi.dylib");
      // Leave room in the load commands so that apps can rewrite the install
      // name (Flutter: @rpath/rawler_capi.framework/rawler_capi, flutter test:
      // an absolute path) with install_name_tool.
      println!("cargo:rustc-cdylib-link-arg=-Wl,-headerpad_max_install_names");
    }
    "android" | "linux" => println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,librawler_capi.so"),
    _ => {}
  }
}
