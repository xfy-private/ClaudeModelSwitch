// 构建辅助脚本。
//
// 历史上这里放置过 libgcc_eh.a 的 workaround（见 findings.md §1）：
// 本机 TDM-GCC 10.3 缺 libgcc_eh.a，而 rustc 的 windows-gnu 链接行恒带 -lgcc_eh。
// 现已在 TDM-GCC 库目录内补齐空归档（社区标准修复），项目侧无需任何 hack。
fn main() {}
