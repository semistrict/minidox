# Third-party software

The root `LICENSE` applies to the original minidox code. It does not replace the
licenses of the vendored upstream projects listed below. Distributions must keep
the applicable license and attribution files with those projects.

| Component | Upstream revision | License | Local license text | Notes |
| --- | --- | --- | --- | --- |
| RedoxFS | `7872ef8bc605e558da1255a9b0af3218cc993f58` | MIT | `vendor/redoxfs/LICENSE` | Vendored with subtree provenance. |
| Redox kernel | `d50854b68dcf04a8554ec08e94f2e23213aae5c1` | MIT | `vendor/redox-kernel/LICENSE` | Vendored source reference with subtree provenance. |
| Cloud Hypervisor | `75afe33d1685d8e1d205d92085011cb863be196f` | Apache-2.0 and BSD-3-Clause, as identified per file | `vendor/cloud-hypervisor/LICENSES/` | Contains downstream modifications; each file's SPDX identifier controls its terms. |
| libkrun devices | `1819e4f6226d5f0d67eb9fdb62c21238e36b29fc` | Apache-2.0 | `vendor/libkrun-devices/LICENSE` | Vendored source reference with subtree provenance. |

Cargo registry dependencies are not copied into this source repository. Their
license expressions include permissive terms compatible with Apache-2.0. The
resolved graph also contains `epoll` and `fdt` under MPL-2.0. Anyone distributing
compiled artifacts must review and satisfy all dependency licenses for that
artifact and target; this source-tree inventory is not a binary-distribution
notice bundle.
