// Native-library reproduction of MOPT aa4d2d11.
// Map the supplied AIG once with the author emap2, then optimize the resulting G0.
// No FULL-186 port, rescaled areas, or external timing feedback.
#include <chrono>
#include <ctime>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <map>
#include <stdexcept>
#include <unordered_set>
#include <nlohmann/json.hpp>
#include <lorina/genlib.hpp>
#include <mockturtle/io/genlib_reader.hpp>
#include <mockturtle/networks/scg.hpp>
#include <mockturtle/algorithms/emap2.hpp>
#include <lorina/aiger.hpp>
#include <mockturtle/io/aiger_reader.hpp>
#include <mockturtle/io/write_blif.hpp>
#include "reference_optional_fix.hpp"

using json = nlohmann::json;
using namespace mockturtle;
using Network = scopt::scg_network;
namespace fs = std::filesystem;
constexpr double AREA_UNITS_PER_UM2 = 1.0;

void save(const fs::path& p, const json& j) { std::ofstream s(p); s << std::setw(2) << j << '\n'; }
void ensure(bool b, const std::string& s) { if (!b) throw std::runtime_error(s); }

std::vector<gate> read_gates(const std::string& p) {
  std::vector<gate> gates;
  ensure(lorina::read_genlib(p, genlib_reader(gates)) == lorina::return_code::success, "GENLIB read failed");
  ensure(gates.size() == 47, "expected original author 47-entry GENLIB");
  return gates;
}

json stats(const Network& n) {return {{"area",n.compute_area()/AREA_UNITS_PER_UM2},{"delay",n.compute_worst_delay()},{"gates",n.num_gates()}};}

void snapshot(const Network& n, const fs::path& p) {
  json j={{"pis",json::array()},{"pos",json::array()},{"cells",json::array()}};
  n.foreach_pi([&](auto v){j["pis"].push_back(v);});
  n.foreach_po([&](auto f){j["pos"].push_back({{"node",n.get_node(f)},{"complement",n.is_complemented(f)}});});
  n.foreach_gate([&](auto v){
    auto const& g=n.get_binding(v);
    json row={{"node",v},{"cell",g.name},{"area",g.area},{"inputs",json::array()}};
    n.foreach_fanin(v,[&](auto f,auto i){row["inputs"].push_back({{"node",n.get_node(f)},
      {"complement",n.is_complemented(f)},{"pin",g.pins.at(i).name},
      {"rise",g.pins.at(i).rise_block_delay},{"fall",g.pins.at(i).fall_block_delay}});});
    j["cells"].push_back(row);
  });
  save(p,j);
}

int main(int argc,char**argv) {
  try {
    ensure(argc>=5,"usage: import GENLIB INPUT_AIG OUT_DIR | optimize GENLIB INPUT_AIG OUT_DIR DB_PREFIX");
    const std::string mode=argv[1];auto gates=read_gates(argv[2]);
    ensure(argc>=5,"missing output directory");fs::path out=argv[4];fs::create_directories(out);
    aig_network aig;
    ensure(lorina::read_aiger(argv[3], aiger_reader(aig)) == lorina::return_code::success, "AIG read failed");
    tech_library_params tps;
    tech_library<5, classification_type::np_configurations> tech(gates,tps);
    scopt::emap2_params mps;
    mps.required_time=std::numeric_limits<float>::max();
    mps.area_oriented_mapping=true;
    auto n=cleanup_scg(scopt::emap2_klut(aig,tech,mps));
    auto initial=stats(n);
    write_blif(n,(out/"g0.blif").string());
    snapshot(n,out/"g0.binding.json");

    json result={{"stage","imported"},{"g0",initial},{"rounds",json::array()}};
    save(out/"summary.json",result);
    if(mode=="import")return 0;
    ensure(mode=="optimize"&&argc==6,"expected optimize and database prefix");
    pLibrary_t db(argv[5]);ensure(!db._areas.empty()&&db._areas.size()==db._idlists.size(),"invalid database");
    rewrub_sc_params ps;ps.preserve_depth=true;ps.delay_awareness=true;ps.max_divisors=256;ps.random_seed=5;
    ps.eps_str*=AREA_UNITS_PER_UM2;ps.eps_fun*=AREA_UNITS_PER_UM2;
    auto begin=std::chrono::steady_clock::now();auto cpu=std::clock();
    double best_area=initial["area"]; unsigned stale=0;
    for(unsigned round=1;round<=100;++round) {
      
      rewrub_sc<10,10>(n,db,ps);
      double seconds=double(std::clock()-cpu)/CLOCKS_PER_SEC;
      auto row=stats(n);row["round"]=round;row["cpu_sec"]=seconds;
      auto filename="round_"+std::to_string(round)+".blif";
      write_blif(n,(out/filename).string());row["netlist"]=fs::absolute(out/filename).string();
      snapshot(n,out/("round_"+std::to_string(round)+".binding.json"));
      result["rounds"].push_back(row);result["stage"]="running";save(out/"summary.json",result);
      std::cout<<"ROUND "<<row.dump()<<std::endl;
      if(n.compute_worst_delay()<=double(initial["delay"])+1e-9 && n.compute_area()<best_area-1e-9) {
        best_area=n.compute_area(); stale=0;
      } else ++stale;
      if(round>=10 && stale>=3) {result["stop_reason"]="area_stagnation";break;}
      if(round==100) result["stop_reason"]="round_cap";
    }
    result["stage"]="complete";result["final"]=stats(n);
    result["cpu_sec"]=double(std::clock()-cpu)/CLOCKS_PER_SEC;
    result["wall_sec"]=std::chrono::duration<double>(std::chrono::steady_clock::now()-begin).count();
    write_blif(n,(out/"final.blif").string());save(out/"summary.json",result);
    return 0;
  }catch(std::exception const&e){std::cerr<<"ERROR: "<<e.what()<<std::endl;return 1;}
}
