# Hardware inventory and package closure — dk-82y3 (82Y3)

Observed 2026-09-07T10:18:48Z by `scripts/hardware-inventory.sh`; the closure joined by
`scripts/hardware-closure.sh` against Ubuntu **resolute** snapshot **20260905T011226Z**
(`https://snapshot.ubuntu.com/ubuntu/20260905T011226Z/dists/resolute/Contents-amd64.gz`, the whole-archive file->package index — one
fetch, then offline). Kernel package family answering here: **7.0.0-14-generic**. The running kernel when
the inventory was taken was 6.17.0-41-generic (journalctl -k -b); the module *names* are
what carries across, not that version.

## The machine

- CPU 13th Gen Intel(R) Core(TM) i7-13700H (GenuineIntel, microcode 0x6134), UEFI: True
- GPUs: card1 nvidia (connected: none), card2 i915 (connected: eDP-2)
- Network: docker0 none, wlp55s0 rtw89_8852ce (wireless); regulatory domain 00
- Audio: NVidia: HDA-Intel - HDA NVidia; PCH: HDA-Intel - HDA Intel PCH
- Storage: nvme0n1 nvme, nvme1n1 nvme
- Input: AT Translated Set 2 keyboard, ELAN06FA:00 04F3:3280 Mouse, ELAN06FA:00 04F3:3280 Touchpad, HDA Intel PCH HDMI/DP,pcm=3, HDA Intel PCH HDMI/DP,pcm=7, HDA Intel PCH HDMI/DP,pcm=8, HDA Intel PCH HDMI/DP,pcm=9, HDA Intel PCH Headphone …

## Firmware the kernel actually loaded on this boot -> package

| firmware | package in resolute |
|---|---|
| cirrus/cs35l41-dsp1-spk-prot-17aa38b5-spkid1-l0.bin | linux-firmware-misc |
| cirrus/cs35l41-dsp1-spk-prot-17aa38b5-spkid1-r0.bin | linux-firmware-misc |
| cirrus/cs35l41-dsp1-spk-prot-17aa38b5.wmfw | linux-firmware-misc |
| i915/adlp_dmc.bin | linux-firmware-intel-graphics |
| i915/adlp_guc_70.bin | linux-firmware-intel-graphics |
| i915/tgl_huc.bin | linux-firmware-intel-graphics |
| rtl_bt/rtl8852cu_config.bin | linux-firmware-realtek |
| rtl_bt/rtl8852cu_fw_v2.bin | linux-firmware-realtek |
| rtw89/rtw8852c_fw-2.bin | linux-firmware-realtek |

## Modules

251 loaded modules; 248 are provided by a
resolute package, 3 are not.

| module | package |
|---|---|
| 842_compress | linux-modules-7.0.0-14-generic |
| 842_decompress | linux-modules-7.0.0-14-generic |
| ac97_bus | linux-modules-7.0.0-14-generic |
| acpi_pad | linux-modules-7.0.0-14-generic |
| acpi_thermal_rel | linux-modules-7.0.0-14-generic |
| aesni_intel | linux-modules-7.0.0-14-generic |
| af_alg | linux-modules-7.0.0-14-generic |
| algif_hash | linux-modules-7.0.0-14-generic |
| algif_skcipher | linux-modules-7.0.0-14-generic |
| arp_tables | linux-modules-7.0.0-14-generic |
| autofs4 | linux-modules-7.0.0-14-generic |
| binfmt_misc | linux-modules-7.0.0-14-generic |
| blake2b_generic | **MISSING** |
| bluetooth | linux-modules-7.0.0-14-generic |
| bnep | linux-modules-7.0.0-14-generic |
| br_netfilter | linux-modules-7.0.0-14-generic |
| bridge | linux-modules-7.0.0-14-generic |
| btbcm | linux-modules-7.0.0-14-generic |
| btintel | linux-modules-7.0.0-14-generic |
| btmtk | linux-modules-7.0.0-14-generic |
| btrfs | linux-modules-7.0.0-14-generic |
| btrtl | linux-modules-7.0.0-14-generic |
| btusb | linux-modules-7.0.0-14-generic |
| ccm | linux-modules-7.0.0-14-generic |
| cec | linux-modules-7.0.0-14-generic |
| cfg80211 | linux-main-modules-iwlwifi-7.0.0-14-generic, linux-modules-7.0.0-14-generic |
| cmac | linux-modules-7.0.0-14-generic |
| cmdlinepart | linux-modules-7.0.0-14-generic |
| coretemp | linux-modules-7.0.0-14-generic |
| cqhci | linux-modules-7.0.0-14-generic |
| crc8 | linux-modules-7.0.0-14-generic |
| cs_dsp | linux-modules-7.0.0-14-generic |
| dm_log | linux-modules-7.0.0-14-generic |
| dm_mirror | linux-modules-7.0.0-14-generic |
| dm_region_hash | linux-modules-7.0.0-14-generic |
| dmi_sysfs | linux-modules-7.0.0-14-generic |
| drm_buddy | linux-modules-7.0.0-14-generic |
| drm_display_helper | linux-modules-7.0.0-14-generic |
| drm_exec | linux-modules-7.0.0-14-generic |
| drm_gpusvm_helper | linux-modules-7.0.0-14-generic |
| drm_gpuvm | linux-modules-7.0.0-14-generic |
| drm_suballoc_helper | linux-modules-7.0.0-14-generic |
| drm_ttm_helper | linux-modules-7.0.0-14-generic |
| efi_pstore | linux-modules-7.0.0-14-generic |
| exfat | linux-modules-7.0.0-14-generic |
| firmware_attributes_class | linux-modules-7.0.0-14-generic |
| ghash_clmulni_intel | linux-modules-7.0.0-14-generic |
| gpu_sched | linux-modules-7.0.0-14-generic |
| hid | linux-modules-7.0.0-14-generic |
| hid_generic | linux-modules-7.0.0-14-generic |
| hid_multitouch | linux-modules-7.0.0-14-generic |
| i2c_algo_bit | linux-modules-7.0.0-14-generic |
| i2c_hid | linux-modules-7.0.0-14-generic |
| i2c_hid_acpi | linux-modules-7.0.0-14-generic |
| i2c_i801 | linux-modules-7.0.0-14-generic |
| i2c_mux | linux-modules-7.0.0-14-generic |
| i2c_smbus | linux-modules-7.0.0-14-generic |
| i915 | linux-modules-7.0.0-14-generic |
| ideapad_laptop | linux-modules-7.0.0-14-generic |
| idma64 | linux-modules-7.0.0-14-generic |
| igen6_edac | linux-modules-7.0.0-14-generic |
| input_leds | linux-modules-7.0.0-14-generic |
| int3400_thermal | linux-modules-7.0.0-14-generic |
| int3403_thermal | linux-modules-7.0.0-14-generic |
| int340x_thermal_zone | linux-modules-7.0.0-14-generic |
| intel_cstate | linux-modules-7.0.0-14-generic |
| intel_lpss | linux-modules-7.0.0-14-generic |
| intel_lpss_pci | linux-modules-7.0.0-14-generic |
| intel_pmc_core | linux-modules-7.0.0-14-generic |
| intel_pmc_ssram_telemetry | linux-modules-7.0.0-14-generic |
| intel_powerclamp | linux-modules-7.0.0-14-generic |
| intel_rapl_common | linux-modules-7.0.0-14-generic |
| intel_rapl_msr | linux-modules-7.0.0-14-generic |
| intel_tcc_cooling | linux-modules-7.0.0-14-generic |
| intel_uncore_frequency | linux-modules-7.0.0-14-generic |
| intel_uncore_frequency_common | linux-modules-7.0.0-14-generic |
| intel_vsec | linux-modules-7.0.0-14-generic |
| ip6_tables | linux-modules-7.0.0-14-generic |
| ip6table_filter | linux-modules-7.0.0-14-generic |
| ip_set | linux-modules-7.0.0-14-generic |
| ip_tables | linux-modules-7.0.0-14-generic |
| iptable_filter | linux-modules-7.0.0-14-generic |
| irqbypass | linux-modules-7.0.0-14-generic |
| joydev | linux-modules-7.0.0-14-generic |
| kvm | linux-modules-7.0.0-14-generic |
| kvm_intel | linux-modules-7.0.0-14-generic |
| legion_laptop | out-of-tree (DKMS on the inventoried host; not expected in the base) |
| lenovo_wmi_capdata01 | **MISSING** |
| lenovo_wmi_events | linux-modules-7.0.0-14-generic |
| lenovo_wmi_gamezone | linux-modules-7.0.0-14-generic |
| lenovo_wmi_helpers | linux-modules-7.0.0-14-generic |
| lenovo_wmi_hotkey_utilities | linux-modules-7.0.0-14-generic |
| lenovo_wmi_other | linux-modules-7.0.0-14-generic |
| libarc4 | linux-modules-7.0.0-14-generic |
| llc | linux-modules-7.0.0-14-generic |
| lp | linux-modules-7.0.0-14-generic |
| lz4_compress | linux-modules-7.0.0-14-generic |
| lz4hc_compress | linux-modules-7.0.0-14-generic |
| mac80211 | linux-main-modules-iwlwifi-7.0.0-14-generic, linux-modules-7.0.0-14-generic |
| mac_hid | linux-modules-7.0.0-14-generic |
| mc | linux-modules-7.0.0-14-generic |
| mei | linux-modules-7.0.0-14-generic |
| mei_hdcp | linux-modules-7.0.0-14-generic |
| mei_me | linux-modules-7.0.0-14-generic |
| mei_pxp | linux-modules-7.0.0-14-generic |
| msr | linux-modules-7.0.0-14-generic |
| mtd | linux-modules-7.0.0-14-generic |
| nf_conntrack | linux-modules-7.0.0-14-generic |
| nf_defrag_ipv4 | linux-modules-7.0.0-14-generic |
| nf_defrag_ipv6 | linux-modules-7.0.0-14-generic |
| nf_nat | linux-modules-7.0.0-14-generic |
| nf_tables | linux-modules-7.0.0-14-generic |
| nfnetlink | linux-modules-7.0.0-14-generic |
| nft_chain_nat | linux-modules-7.0.0-14-generic |
| nft_compat | linux-modules-7.0.0-14-generic |
| nls_iso8859_1 | linux-modules-7.0.0-14-generic |
| nvidia | linux-modules-nvidia-580-open-7.0.0-14-generic, linux-modules-nvidia-580-server-open-7.0.0-14-generic, linux-modules-nvidia-595-open-7.0.0-14-generic, linux-modules-nvidia-595-server-open-7.0.0-14-generic, linux-objects-nvidia-580-open-7.0.0-14-generic, linux-objects-nvidia-580-server-open-7.0.0-14-generic, linux-objects-nvidia-595-open-7.0.0-14-generic, linux-objects-nvidia-595-server-open-7.0.0-14-generic |
| nvidia_drm | linux-modules-nvidia-580-open-7.0.0-14-generic, linux-modules-nvidia-580-server-open-7.0.0-14-generic, linux-modules-nvidia-595-open-7.0.0-14-generic, linux-modules-nvidia-595-server-open-7.0.0-14-generic, linux-objects-nvidia-580-open-7.0.0-14-generic, linux-objects-nvidia-580-server-open-7.0.0-14-generic, linux-objects-nvidia-595-open-7.0.0-14-generic, linux-objects-nvidia-595-server-open-7.0.0-14-generic |
| nvidia_modeset | linux-modules-nvidia-580-open-7.0.0-14-generic, linux-modules-nvidia-580-server-open-7.0.0-14-generic, linux-modules-nvidia-595-open-7.0.0-14-generic, linux-modules-nvidia-595-server-open-7.0.0-14-generic, linux-objects-nvidia-580-open-7.0.0-14-generic, linux-objects-nvidia-580-server-open-7.0.0-14-generic, linux-objects-nvidia-595-open-7.0.0-14-generic, linux-objects-nvidia-595-server-open-7.0.0-14-generic |
| nvidia_uvm | linux-modules-nvidia-580-open-7.0.0-14-generic, linux-modules-nvidia-580-server-open-7.0.0-14-generic, linux-modules-nvidia-595-open-7.0.0-14-generic, linux-modules-nvidia-595-server-open-7.0.0-14-generic, linux-objects-nvidia-580-open-7.0.0-14-generic, linux-objects-nvidia-580-server-open-7.0.0-14-generic, linux-objects-nvidia-595-open-7.0.0-14-generic, linux-objects-nvidia-595-server-open-7.0.0-14-generic |
| nvidia_wmi_ec_backlight | linux-modules-7.0.0-14-generic |
| nvme | linux-modules-7.0.0-14-generic |
| nvme_auth | linux-modules-7.0.0-14-generic |
| nvme_core | linux-modules-7.0.0-14-generic |
| nvme_keyring | linux-modules-7.0.0-14-generic |
| overlay | linux-modules-7.0.0-14-generic |
| parport | linux-modules-7.0.0-14-generic |
| parport_pc | linux-modules-7.0.0-14-generic |
| pinctrl_tigerlake | linux-modules-7.0.0-14-generic |
| platform_profile | linux-modules-7.0.0-14-generic |
| platform_temperature_control | linux-modules-7.0.0-14-generic |
| pmt_class | linux-modules-7.0.0-14-generic |
| pmt_discovery | linux-modules-7.0.0-14-generic |
| pmt_telemetry | linux-modules-7.0.0-14-generic |
| polyval_clmulni | **MISSING** |
| ppdev | linux-modules-7.0.0-14-generic |
| processor_thermal_device | linux-modules-7.0.0-14-generic |
| processor_thermal_device_pci | linux-modules-7.0.0-14-generic |
| processor_thermal_mbox | linux-modules-7.0.0-14-generic |
| processor_thermal_power_floor | linux-modules-7.0.0-14-generic |
| processor_thermal_rapl | linux-modules-7.0.0-14-generic |
| processor_thermal_rfim | linux-modules-7.0.0-14-generic |
| processor_thermal_soc_slider | linux-modules-7.0.0-14-generic |
| processor_thermal_wt_hint | linux-modules-7.0.0-14-generic |
| processor_thermal_wt_req | linux-modules-7.0.0-14-generic |
| qrtr | linux-modules-7.0.0-14-generic |
| raid6_pq | linux-modules-7.0.0-14-generic |
| rapl | linux-modules-7.0.0-14-generic |
| rc_core | linux-modules-7.0.0-14-generic |
| rfcomm | linux-modules-7.0.0-14-generic |
| rtw89_8852c | linux-modules-7.0.0-14-generic |
| rtw89_8852ce | linux-modules-7.0.0-14-generic |
| rtw89_core | linux-modules-7.0.0-14-generic |
| rtw89_pci | linux-modules-7.0.0-14-generic |
| sch_fq_codel | linux-modules-7.0.0-14-generic |
| sdhci | linux-modules-7.0.0-14-generic |
| sdhci_pci | linux-modules-7.0.0-14-generic |
| sdhci_uhs2 | linux-modules-7.0.0-14-generic |
| serial_multi_instantiate | linux-modules-7.0.0-14-generic |
| serio_raw | linux-modules-7.0.0-14-generic |
| snd | linux-modules-7.0.0-14-generic |
| snd_compress | linux-modules-7.0.0-14-generic |
| snd_ctl_led | linux-modules-7.0.0-14-generic |
| snd_hda_codec | linux-modules-7.0.0-14-generic |
| snd_hda_codec_alc269 | linux-modules-7.0.0-14-generic |
| snd_hda_codec_generic | linux-modules-7.0.0-14-generic |
| snd_hda_codec_hdmi | linux-modules-7.0.0-14-generic |
| snd_hda_codec_intelhdmi | linux-modules-7.0.0-14-generic |
| snd_hda_codec_nvhdmi | linux-modules-7.0.0-14-generic |
| snd_hda_codec_realtek_lib | linux-modules-7.0.0-14-generic |
| snd_hda_core | linux-modules-7.0.0-14-generic |
| snd_hda_ext_core | linux-modules-7.0.0-14-generic |
| snd_hda_intel | linux-modules-7.0.0-14-generic |
| snd_hda_scodec_component | linux-modules-7.0.0-14-generic |
| snd_hda_scodec_cs35l41 | linux-modules-7.0.0-14-generic |
| snd_hda_scodec_cs35l41_i2c | linux-modules-7.0.0-14-generic |
| snd_hda_scodec_cs35l41_spi | linux-modules-7.0.0-14-generic |
| snd_hrtimer | linux-modules-7.0.0-14-generic |
| snd_hwdep | linux-modules-7.0.0-14-generic |
| snd_intel_dspcfg | linux-modules-7.0.0-14-generic |
| snd_intel_sdw_acpi | linux-modules-7.0.0-14-generic |
| snd_pcm | linux-modules-7.0.0-14-generic |
| snd_pcm_dmaengine | linux-modules-7.0.0-14-generic |
| snd_rawmidi | linux-modules-7.0.0-14-generic |
| snd_seq | linux-modules-7.0.0-14-generic |
| snd_seq_device | linux-modules-7.0.0-14-generic |
| snd_seq_dummy | linux-modules-7.0.0-14-generic |
| snd_seq_midi | linux-modules-7.0.0-14-generic |
| snd_seq_midi_event | linux-modules-7.0.0-14-generic |
| snd_soc_acpi | linux-modules-7.0.0-14-generic |
| snd_soc_acpi_intel_match | linux-modules-7.0.0-14-generic |
| snd_soc_acpi_intel_sdca_quirks | linux-modules-7.0.0-14-generic |
| snd_soc_avs | linux-modules-7.0.0-14-generic |
| snd_soc_core | linux-modules-7.0.0-14-generic |
| snd_soc_cs35l41_lib | linux-modules-7.0.0-14-generic |
| snd_soc_cs_amp_lib | linux-modules-7.0.0-14-generic |
| snd_soc_hda_codec | linux-modules-7.0.0-14-generic |
| snd_soc_hdac_hda | linux-modules-7.0.0-14-generic |
| snd_soc_sdca | linux-modules-7.0.0-14-generic |
| snd_sof | linux-modules-7.0.0-14-generic |
| snd_sof_intel_hda | linux-modules-7.0.0-14-generic |
| snd_sof_intel_hda_common | linux-modules-7.0.0-14-generic |
| snd_sof_intel_hda_generic | linux-modules-7.0.0-14-generic |
| snd_sof_intel_hda_mlink | linux-modules-7.0.0-14-generic |
| snd_sof_intel_hda_sdw_bpt | linux-modules-7.0.0-14-generic |
| snd_sof_pci | linux-modules-7.0.0-14-generic |
| snd_sof_pci_intel_cnl | linux-modules-7.0.0-14-generic |
| snd_sof_pci_intel_tgl | linux-modules-7.0.0-14-generic |
| snd_sof_utils | linux-modules-7.0.0-14-generic |
| snd_sof_xtensa_dsp | linux-modules-7.0.0-14-generic |
| snd_timer | linux-modules-7.0.0-14-generic |
| soundcore | linux-modules-7.0.0-14-generic |
| soundwire_bus | linux-modules-7.0.0-14-generic |
| soundwire_cadence | linux-modules-7.0.0-14-generic |
| soundwire_generic_allocation | linux-modules-7.0.0-14-generic |
| soundwire_intel | linux-modules-7.0.0-14-generic |
| sparse_keymap | linux-modules-7.0.0-14-generic |
| spi_intel | linux-modules-7.0.0-14-generic |
| spi_intel_pci | linux-modules-7.0.0-14-generic |
| spi_nor | linux-modules-7.0.0-14-generic |
| stp | linux-modules-7.0.0-14-generic |
| thunderbolt | linux-modules-7.0.0-14-generic |
| ttm | linux-modules-7.0.0-14-generic |
| typec | linux-modules-7.0.0-14-generic |
| typec_ucsi | linux-modules-7.0.0-14-generic |
| uas | linux-modules-7.0.0-14-generic |
| ucsi_acpi | linux-modules-7.0.0-14-generic |
| usb_storage | linux-modules-7.0.0-14-generic |
| usbhid | linux-modules-7.0.0-14-generic |
| uvc | linux-modules-7.0.0-14-generic |
| uvcvideo | linux-modules-7.0.0-14-generic |
| video | linux-modules-7.0.0-14-generic |
| videobuf2_common | linux-modules-7.0.0-14-generic |
| videobuf2_memops | linux-modules-7.0.0-14-generic |
| videobuf2_v4l2 | linux-modules-7.0.0-14-generic |
| videobuf2_vmalloc | linux-modules-7.0.0-14-generic |
| videodev | linux-modules-7.0.0-14-generic |
| wmi | linux-modules-7.0.0-14-generic |
| wmi_bmof | linux-modules-7.0.0-14-generic |
| x86_pkg_temp_thermal | linux-modules-7.0.0-14-generic |
| x_tables | linux-modules-7.0.0-14-generic |
| xe | linux-modules-7.0.0-14-generic |
| xfrm_algo | linux-modules-7.0.0-14-generic |
| xfrm_user | linux-modules-7.0.0-14-generic |
| xor | linux-modules-7.0.0-14-generic |
| xt_MASQUERADE | linux-modules-7.0.0-14-generic |
| xt_addrtype | linux-modules-7.0.0-14-generic |
| xt_conntrack | linux-modules-7.0.0-14-generic |
| xt_set | linux-modules-7.0.0-14-generic |
| zram | linux-modules-7.0.0-14-generic |
| zstd | linux-modules-7.0.0-14-generic |

## Firmware declared by those modules (whether or not this machine asked for it)

132 exact paths checked, 21 not in resolute; 2 declarations
are glob patterns and are covered by the loaded list above.

| firmware | package |
|---|---|
| intel/avs/adl/dsp_basefw.bin | **MISSING** |
| intel/avs/adl_n/dsp_basefw.bin | **MISSING** |
| intel/avs/ehl/dsp_basefw.bin | **MISSING** |
| intel/avs/icl/dsp_basefw.bin | **MISSING** |
| intel/avs/jsl/dsp_basefw.bin | **MISSING** |
| intel/avs/lkf/dsp_basefw.bin | **MISSING** |
| intel/avs/tgl/dsp_basefw.bin | **MISSING** |
| intel/fcl/dsp_basefw.bin | **MISSING** |
| nvidia/610.43.02/gsp_ga10x.bin | **MISSING** |
| nvidia/610.43.02/gsp_tu10x.bin | **MISSING** |
| nvidia/610.43.02/ucodes_ga10x.bin | **MISSING** |
| nvidia/610.43.02/ucodes_tu10x.bin | **MISSING** |
| rtl_bt/rtl8723cs_cg_config.bin | **MISSING** |
| rtl_bt/rtl8723cs_cg_fw.bin | **MISSING** |
| rtl_bt/rtl8723cs_vf_config.bin | **MISSING** |
| rtl_bt/rtl8723cs_vf_fw.bin | **MISSING** |
| rtl_bt/rtl8723ds_config.bin | **MISSING** |
| rtl_bt/rtl8723ds_fw.bin | **MISSING** |
| rtl_bt/rtl8761a_config.bin | **MISSING** |
| rtl_bt/rtl8852bs_config.bin | **MISSING** |
| rtl_bt/rtl8852bs_fw.bin | **MISSING** |

## Verdict

- firmware actually in use that no package provides: **0**
- loaded modules no package provides: **3** — blake2b_generic, lenovo_wmi_capdata01, polyval_clmulni

Every MISSING row is either a seed to add or a recorded reason; a module with no package is usually
out-of-tree (DKMS, a container/virtualisation module) rather than a gap in the base.
