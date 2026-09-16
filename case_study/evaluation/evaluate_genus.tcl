# Mapped-as-is evaluation only. No synthesis or optimization command.
set out $::env(EGG_USB_GENUS_OUT)
set_db library [split $::env(EGG_USB_GENUS_LIBS) "|"]
set_db max_cpus_per_server 1
set_db enable_ui_precision true
set_db ui_precision_timing 8
set_db ui_precision_capacitance 8
set_db ui_precision_power 8
set ordinal 0
foreach line [split $::env(EGG_USB_GENUS_INPUTS) "\n"] {
    lassign [split $line "|"] label netlist top
    if {$label eq ""} {continue}
    if {$ordinal > 0} {delete_obj [get_db designs]}
    set dir [file join $out $label]
    file mkdir $dir
    read_hdl -sv $netlist
    elaborate $top
    check_design -unresolved > [file join $dir unresolved.rpt]
    set f [open [file join $dir instances_before.tsv] w]
    foreach inst [get_db insts] {puts $f "[get_db $inst .name]\t[get_db $inst .base_cell.name]"}
    close $f
    create_clock -name vclk -period 1000
    set_input_delay 0 -clock vclk [all_inputs]
    set_output_delay 0 -clock vclk [all_outputs]
    set_driving_cell -lib_cell BUFx2_ASAP7_75t_R -input_transition_rise 0 -input_transition_fall 0 [all_inputs]
    set_load 5.76 [all_outputs]
    # Explicit common activity: 100 MHz is 0.1 transition per 1000 ps cycle.
    set_activity -activity_type default -pin_types primary_input -duty 0.5 -freq 1.0e8
    set_activity -activity_type default -pin_types comb_out -duty 0.5 -freq 1.0e8
    report_units > [file join $dir units.rpt]
    report_area > [file join $dir area.rpt]
    report_timing -from [all_inputs] -to [all_outputs] -max_paths 10 > [file join $dir timing.rpt]
    report_power > [file join $dir power.rpt]
    report_power -by_leaf_instance -unit uW > [file join $dir power_by_leaf_instance.rpt]
    set af [open [file join $dir activity_config.rpt] w]
    puts $af "primary_input_duty 0.5"
    puts $af "primary_input_frequency_hz 1.0e8"
    puts $af "comb_output_default_duty 0.5"
    puts $af "comb_output_default_frequency_hz 1.0e8"
    puts $af "virtual_clock_period_ps 1000"
    puts $af "transitions_per_cycle 0.1"
    close $af
    set paths [report_timing -from [all_inputs] -to [all_outputs] -max_paths 1 -collection]
    set f [open [file join $dir metrics.rpt] w]
    foreach attr {.slack .arrival .required_time .path_delay} {
        puts $f "$attr [get_db $paths $attr]"
    }
    puts $f "instances [llength [get_db insts]]"
    puts $f "input_ports [sizeof_collection [all_inputs]]"
    puts $f "output_ports [sizeof_collection [all_outputs]]"
    close $f
    write_sdc > [file join $dir boundary.sdc]
    write_hdl > [file join $dir genus_out.v]
    set f [open [file join $dir instances_after.tsv] w]
    foreach inst [get_db insts] {puts $f "[get_db $inst .name]\t[get_db $inst .base_cell.name]"}
    close $f
    puts "EGG_USB_GENUS_POINT_DONE $label"
    incr ordinal
}
puts "EGG_USB_GENUS_DONE $ordinal"
exit
