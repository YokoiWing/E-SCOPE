# Mapped-as-is Genus evaluation for the Figure 7 reproduction pipeline.
# Each line in E_SCOPE_BATCH_ITEMS_FILE is:
# label|absolute_netlist|top_module|absolute_report_directory
set lib $::env(E_SCOPE_GENUS_LIBERTY)
set items_channel [open $::env(E_SCOPE_BATCH_ITEMS_FILE) r]
set batch_items [read $items_channel]
close $items_channel

set_db init_lib_search_path [list [file dirname $lib]]
set_db library [list $lib]
set_db max_cpus_per_server 1
set_db enable_ui_precision true
set_db ui_precision_timing 8
set_db ui_precision_power 8
set_db ui_precision_capacitance 8

set ordinal 0
foreach line [split $batch_items "\n"] {
  if {[string trim $line] eq ""} { continue }
  lassign [split $line "|"] label netlist top report_dir
  if {$ordinal > 0} { delete_obj [get_db designs] }
  file mkdir $report_dir
  read_hdl -sv $netlist
  elaborate $top
  create_clock -name vclk -period 1000
  set_input_delay 0 -clock vclk [all_inputs]
  set_output_delay 0 -clock vclk [all_outputs]
  set_driving_cell -lib_cell BUFx2_ASAP7_6t_L [all_inputs]
  set_load 5.76 [all_outputs]
  set_activity -activity_type default -pin_types primary_input -duty 0.5 -freq 1.0e8
  set_activity -activity_type default -pin_types comb_out -duty 0.5 -freq 1.0e8
  report_area > [file join $report_dir area.rpt]
  report_timing -from [all_inputs] -to [all_outputs] -max_paths 1 > [file join $report_dir timing.rpt]
  report_power > [file join $report_dir power.rpt]
  puts "E_SCOPE_BATCH_POINT_DONE label=$label top=$top"
  incr ordinal
}
puts "E_SCOPE_BATCH_DONE points=$ordinal"
exit
