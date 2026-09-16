# Third-party software and data

This inventory records what is present in this artifact and what a user must
provide. A public download location alone is not treated as redistribution
permission.

| Component | Source/version | License or rights status | Artifact treatment |
|---|---|---|---|
| Mockturtle/AreaPMO snapshot | `costamag/mockturtle`, PMO commit `02fe046a8e14869624b834b27a6c110344cdb3e5`; recovered mapper reference `8062d03c59cb7bfde005283457819e327536f443` | MIT for the vendored Mockturtle tree; notice retained in `case_study/areapmo/source/vendor/LICENSE` | Source subset retained; no precompiled AreaPMO executable |
| fmt | Vendored dependency of the Mockturtle snapshot | BSD 2-Clause; notice retained in `case_study/areapmo/source/vendor/lib/fmt/LICENSE.rst` | Source retained |
| parallel-hashmap and embedded Abseil portions | Vendored dependency of the Mockturtle snapshot | Apache-2.0; header notices retained and full terms copied to `licenses/Apache-2.0.txt` | Source retained |
| Other Mockturtle header dependencies (`kitty`, `lorina`, `bill`, `abcsat`, `nlohmann/json`) | Vendored dependency closure | Mockturtle-family code is covered by the retained MIT notice; embedded SAT and JSON files retain their per-file notices | Source retained; per-file notices are authoritative |
| Berkeley ABC | <https://github.com/berkeley-abc/abc>; local historical fork `costamag/abc` commit `6c6260465efe7b38ee207a5d018da5e5c5eb52fe` | Permissive UC Berkeley terms copied to `licenses/Berkeley-ABC.txt` | Binary removed; user supplies `ABC_BIN` |
| ASAP7 Liberty NLDM files | ASAP7 standard-cell libraries, file revisions embedded in each `.lib` | BSD 3-Clause, copyright Lawrence T. Clark, Vinay Vashishtha, or Arizona State University; full notice is embedded in each distributed file | The case-study Liberty subset is retained with its notice. The main experiment's full-combinational Liberty is omitted; the user supplies the exact file with SHA-256 `48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd` |
| Cadence Genus 23.14-s090_1 | Cadence Design Systems | Commercial, no redistribution authorization | Program, installation, and license are absent; user supplies a licensed installation |
| IWLS 2005 / OpenCores-derived and EPFL/ISCAS-derived prepared benchmarks | <https://www.iwls.org/iwls2005/benchmarks.html>; public Mockturtle import commit `1fa274d0773a42145fc4610adb0b21b073abcaa7`; EPFL and ISCAS source families recorded in the experiment manifest | Exact upstream conversion and redistribution permission have not been fully established | Prepared case-study inputs and 28 mapped G0 netlists are retained for exact reproduction. Public redistribution remains a release-rights blocker until permission is confirmed or user-fetch preparation replaces these files |
| E-SCOPE Rust/Python code and experiment records | Authors' workspace snapshot; per-file hashes in `case_study/iterative/SOURCE_PROVENANCE.json` | No author-selected repository license has been supplied | No blanket open-source license is asserted. License choice is a publication blocker |

The former public history containing precompiled executables and local path
strings was replaced on 2026-09-16 with an anonymous root commit. The current
`main` history contains no predecessor objects. Prepared benchmark provenance
remains documented above; no broader rights are inferred from public access.
