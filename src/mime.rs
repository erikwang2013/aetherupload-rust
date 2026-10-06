// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! MIME ↔ 扩展名映射与内容类型探测 —— 对应 PHP 版 `MimeType.php` + `mime_content_type()`。
//!
//! 表本身从 PHP 版原样搬运（769 条，保序；PHP 数组的重复键 `tar` 已核对为同值，
//! 保留首个出现位置）。用途只有一个：末块落盘前按**真实内容**反查扩展名，再走一遍
//! 扩展名白/黑名单 —— 客户端声明的 `resource_ext` 不可信（对齐 PHP 的
//! `checkMimeType()`：`MimeType::search(mime_content_type($path))`）。
//!
//! 表以紧凑字符串内联（每行 8 条 `扩展名=类型`），首次查询解析一次并缓存：770 行的
//! 逐行数组会顶破仓库「单文件 500 行」的约定，而数据表的 diff 友好度不如规则一致性重要。

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::OnceLock;

const MIMES_PACKED: &str = "\
    ez=application/andrew-inset aw=application/applixware atom=application/atom+xml atomcat=application/atomcat+xml atomsvc=application/atomsvc+xml ccxml=application/ccxml+xml cdmia=application/cdmi-capability cdmic=application/cdmi-container
    cdmid=application/cdmi-domain cdmio=application/cdmi-object cdmiq=application/cdmi-queue cu=application/cu-seeme davmount=application/davmount+xml dbk=application/docbook+xml dssc=application/dssc+der xdssc=application/dssc+xml
    ecma=application/ecmascript emma=application/emma+xml epub=application/epub+zip exi=application/exi pfr=application/font-tdpfr gml=application/gml+xml gpx=application/gpx+xml gxf=application/gxf
    stk=application/hyperstudio ink=application/inkml+xml ipfix=application/ipfix jar=application/java-archive ser=application/java-serialized-object class=application/java-vm js=application/javascript json=application/json
    jsonml=application/jsonml+json lostxml=application/lost+xml hqx=application/mac-binhex40 cpt=application/mac-compactpro mads=application/mads+xml mrc=application/marc mrcx=application/marcxml+xml ma=application/mathematica
    mathml=application/mathml+xml mbox=application/mbox mscml=application/mediaservercontrol+xml metalink=application/metalink+xml meta4=application/metalink4+xml mets=application/mets+xml mods=application/mods+xml m21=application/mp21
    mp4s=application/mp4 doc=application/msword mxf=application/mxf bin=application/octet-stream oda=application/oda opf=application/oebps-package+xml ogx=application/ogg omdoc=application/omdoc+xml
    onetoc=application/onenote oxps=application/oxps xer=application/patch-ops-error+xml pdf=application/pdf pgp=application/pgp-encrypted asc=application/pgp-signature prf=application/pics-rules p10=application/pkcs10
    p7m=application/pkcs7-mime p7s=application/pkcs7-signature p8=application/pkcs8 ac=application/pkix-attr-cert cer=application/pkix-cert crl=application/pkix-crl pkipath=application/pkix-pkipath pki=application/pkixcmp
    pls=application/pls+xml ai=application/postscript cww=application/prs.cww pskcxml=application/pskc+xml rdf=application/rdf+xml rif=application/reginfo+xml rnc=application/relax-ng-compact-syntax rl=application/resource-lists+xml
    rld=application/resource-lists-diff+xml rs=application/rls-services+xml gbr=application/rpki-ghostbusters mft=application/rpki-manifest roa=application/rpki-roa rsd=application/rsd+xml rss=application/rss+xml sbml=application/sbml+xml
    scq=application/scvp-cv-request scs=application/scvp-cv-response spq=application/scvp-vp-request spp=application/scvp-vp-response sdp=application/sdp setpay=application/set-payment-initiation setreg=application/set-registration-initiation shf=application/shf+xml
    smi=application/smil+xml rq=application/sparql-query srx=application/sparql-results+xml gram=application/srgs grxml=application/srgs+xml sru=application/sru+xml ssdl=application/ssdl+xml ssml=application/ssml+xml
    tei=application/tei+xml tfi=application/thraud+xml tsd=application/timestamped-data plb=application/vnd.3gpp.pic-bw-large psb=application/vnd.3gpp.pic-bw-small pvb=application/vnd.3gpp.pic-bw-var tcap=application/vnd.3gpp2.tcap pwn=application/vnd.3m.post-it-notes
    aso=application/vnd.accpac.simply.aso imp=application/vnd.accpac.simply.imp acu=application/vnd.acucobol atc=application/vnd.acucorp air=application/vnd.adobe.air-application-installer-package+zip fcdt=application/vnd.adobe.formscentral.fcdt fxp=application/vnd.adobe.fxp xdp=application/vnd.adobe.xdp+xml
    xfdf=application/vnd.adobe.xfdf ahead=application/vnd.ahead.space azf=application/vnd.airzip.filesecure.azf azs=application/vnd.airzip.filesecure.azs azw=application/vnd.amazon.ebook acc=application/vnd.americandynamics.acc ami=application/vnd.amiga.ami apk=application/vnd.android.package-archive
    cii=application/vnd.anser-web-certificate-issue-initiation fti=application/vnd.anser-web-funds-transfer-initiation atx=application/vnd.antix.game-component mpkg=application/vnd.apple.installer+xml m3u8=application/vnd.apple.mpegurl swi=application/vnd.aristanetworks.swi iota=application/vnd.astraea-software.iota aep=application/vnd.audiograph
    mpm=application/vnd.blueice.multipass bmi=application/vnd.bmi rep=application/vnd.businessobjects cdxml=application/vnd.chemdraw+xml mmd=application/vnd.chipnuts.karaoke-mmd cdy=application/vnd.cinderella cla=application/vnd.claymore rp9=application/vnd.cloanto.rp9
    c4g=application/vnd.clonk.c4group c11amc=application/vnd.cluetrust.cartomobile-config c11amz=application/vnd.cluetrust.cartomobile-config-pkg csp=application/vnd.commonspace cdbcmsg=application/vnd.contact.cmsg cmc=application/vnd.cosmocaller clkx=application/vnd.crick.clicker clkk=application/vnd.crick.clicker.keyboard
    clkp=application/vnd.crick.clicker.palette clkt=application/vnd.crick.clicker.template clkw=application/vnd.crick.clicker.wordbank wbs=application/vnd.criticaltools.wbs+xml pml=application/vnd.ctc-posml ppd=application/vnd.cups-ppd car=application/vnd.curl.car pcurl=application/vnd.curl.pcurl
    dart=application/vnd.dart rdz=application/vnd.data-vision.rdz uvf=application/vnd.dece.data uvt=application/vnd.dece.ttml+xml uvx=application/vnd.dece.unspecified uvz=application/vnd.dece.zip fe_launch=application/vnd.denovo.fcselayout-link dna=application/vnd.dna
    mlp=application/vnd.dolby.mlp dpg=application/vnd.dpgraph dfac=application/vnd.dreamfactory kpxx=application/vnd.ds-keypoint ait=application/vnd.dvb.ait svc=application/vnd.dvb.service geo=application/vnd.dynageo mag=application/vnd.ecowin.chart
    nml=application/vnd.enliven esf=application/vnd.epson.esf msf=application/vnd.epson.msf qam=application/vnd.epson.quickanime slt=application/vnd.epson.salt ssf=application/vnd.epson.ssf es3=application/vnd.eszigno3+xml ez2=application/vnd.ezpix-album
    ez3=application/vnd.ezpix-package fdf=application/vnd.fdf mseed=application/vnd.fdsn.mseed seed=application/vnd.fdsn.seed gph=application/vnd.flographit ftc=application/vnd.fluxtime.clip fm=application/vnd.framemaker fnc=application/vnd.frogans.fnc
    ltf=application/vnd.frogans.ltf fsc=application/vnd.fsc.weblaunch oas=application/vnd.fujitsu.oasys oa2=application/vnd.fujitsu.oasys2 oa3=application/vnd.fujitsu.oasys3 fg5=application/vnd.fujitsu.oasysgp bh2=application/vnd.fujitsu.oasysprs ddd=application/vnd.fujixerox.ddd
    xdw=application/vnd.fujixerox.docuworks xbd=application/vnd.fujixerox.docuworks.binder fzs=application/vnd.fuzzysheet txd=application/vnd.genomatix.tuxedo ggb=application/vnd.geogebra.file ggt=application/vnd.geogebra.tool gex=application/vnd.geometry-explorer gxt=application/vnd.geonext
    g2w=application/vnd.geoplan g3w=application/vnd.geospace gmx=application/vnd.gmx kml=application/vnd.google-earth.kml+xml kmz=application/vnd.google-earth.kmz gqf=application/vnd.grafeq gac=application/vnd.groove-account ghf=application/vnd.groove-help
    gim=application/vnd.groove-identity-message grv=application/vnd.groove-injector gtm=application/vnd.groove-tool-message tpl=application/vnd.groove-tool-template vcg=application/vnd.groove-vcard hal=application/vnd.hal+xml zmm=application/vnd.handheld-entertainment+xml hbci=application/vnd.hbci
    les=application/vnd.hhe.lesson-player hpgl=application/vnd.hp-hpgl hpid=application/vnd.hp-hpid hps=application/vnd.hp-hps jlt=application/vnd.hp-jlyt pcl=application/vnd.hp-pcl pclxl=application/vnd.hp-pclxl sfd-hdstx=application/vnd.hydrostatix.sof-data
    mpy=application/vnd.ibm.minipay afp=application/vnd.ibm.modcap irm=application/vnd.ibm.rights-management sc=application/vnd.ibm.secure-container icc=application/vnd.iccprofile igl=application/vnd.igloader ivp=application/vnd.immervision-ivp ivu=application/vnd.immervision-ivu
    igm=application/vnd.insors.igm xpw=application/vnd.intercon.formnet i2g=application/vnd.intergeo qbo=application/vnd.intu.qbo qfx=application/vnd.intu.qfx rcprofile=application/vnd.ipunplugged.rcprofile irp=application/vnd.irepository.package+xml xpr=application/vnd.is-xpr
    fcs=application/vnd.isac.fcs jam=application/vnd.jam rms=application/vnd.jcp.javame.midlet-rms jisp=application/vnd.jisp joda=application/vnd.joost.joda-archive ktz=application/vnd.kahootz karbon=application/vnd.kde.karbon chrt=application/vnd.kde.kchart
    kfo=application/vnd.kde.kformula flw=application/vnd.kde.kivio kon=application/vnd.kde.kontour kpr=application/vnd.kde.kpresenter ksp=application/vnd.kde.kspread kwd=application/vnd.kde.kword htke=application/vnd.kenameaapp kia=application/vnd.kidspiration
    kne=application/vnd.kinar skp=application/vnd.koan sse=application/vnd.kodak-descriptor lasxml=application/vnd.las.las+xml lbd=application/vnd.llamagraphics.life-balance.desktop lbe=application/vnd.llamagraphics.life-balance.exchange+xml 123=application/vnd.lotus-1-2-3 apr=application/vnd.lotus-approach
    pre=application/vnd.lotus-freelance nsf=application/vnd.lotus-notes org=application/vnd.lotus-organizer scm=application/vnd.lotus-screencam lwp=application/vnd.lotus-wordpro portpkg=application/vnd.macports.portpkg mcd=application/vnd.mcd mc1=application/vnd.medcalcdata
    cdkey=application/vnd.mediastation.cdkey mwf=application/vnd.mfer mfm=application/vnd.mfmp flo=application/vnd.micrografx.flo igx=application/vnd.micrografx.igx mif=application/vnd.mif daf=application/vnd.mobius.daf dis=application/vnd.mobius.dis
    mbk=application/vnd.mobius.mbk mqy=application/vnd.mobius.mqy msl=application/vnd.mobius.msl plc=application/vnd.mobius.plc txf=application/vnd.mobius.txf mpn=application/vnd.mophun.application mpc=application/vnd.mophun.certificate xul=application/vnd.mozilla.xul+xml
    cil=application/vnd.ms-artgalry cab=application/vnd.ms-cab-compressed xls=application/vnd.ms-excel xlam=application/vnd.ms-excel.addin.macroenabled.12 xlsb=application/vnd.ms-excel.sheet.binary.macroenabled.12 xlsm=application/vnd.ms-excel.sheet.macroenabled.12 xltm=application/vnd.ms-excel.template.macroenabled.12 eot=application/vnd.ms-fontobject
    chm=application/vnd.ms-htmlhelp ims=application/vnd.ms-ims lrm=application/vnd.ms-lrm thmx=application/vnd.ms-officetheme cat=application/vnd.ms-pki.seccat stl=application/vnd.ms-pki.stl ppt=application/vnd.ms-powerpoint ppam=application/vnd.ms-powerpoint.addin.macroenabled.12
    pptm=application/vnd.ms-powerpoint.presentation.macroenabled.12 sldm=application/vnd.ms-powerpoint.slide.macroenabled.12 ppsm=application/vnd.ms-powerpoint.slideshow.macroenabled.12 potm=application/vnd.ms-powerpoint.template.macroenabled.12 mpp=application/vnd.ms-project docm=application/vnd.ms-word.document.macroenabled.12 dotm=application/vnd.ms-word.template.macroenabled.12 wps=application/vnd.ms-works
    wpl=application/vnd.ms-wpl xps=application/vnd.ms-xpsdocument mseq=application/vnd.mseq mus=application/vnd.musician msty=application/vnd.muvee.style taglet=application/vnd.mynfc nlu=application/vnd.neurolanguage.nlu ntf=application/vnd.nitf
    nnd=application/vnd.noblenet-directory nns=application/vnd.noblenet-sealer nnw=application/vnd.noblenet-web ngdat=application/vnd.nokia.n-gage.data n-gage=application/vnd.nokia.n-gage.symbian.install rpst=application/vnd.nokia.radio-preset rpss=application/vnd.nokia.radio-presets edm=application/vnd.novadigm.edm
    edx=application/vnd.novadigm.edx ext=application/vnd.novadigm.ext odc=application/vnd.oasis.opendocument.chart otc=application/vnd.oasis.opendocument.chart-template odb=application/vnd.oasis.opendocument.database odf=application/vnd.oasis.opendocument.formula odft=application/vnd.oasis.opendocument.formula-template odg=application/vnd.oasis.opendocument.graphics
    otg=application/vnd.oasis.opendocument.graphics-template odi=application/vnd.oasis.opendocument.image oti=application/vnd.oasis.opendocument.image-template odp=application/vnd.oasis.opendocument.presentation otp=application/vnd.oasis.opendocument.presentation-template ods=application/vnd.oasis.opendocument.spreadsheet ots=application/vnd.oasis.opendocument.spreadsheet-template odt=application/vnd.oasis.opendocument.text
    odm=application/vnd.oasis.opendocument.text-master ott=application/vnd.oasis.opendocument.text-template oth=application/vnd.oasis.opendocument.text-web xo=application/vnd.olpc-sugar dd2=application/vnd.oma.dd2+xml oxt=application/vnd.openofficeorg.extension pptx=application/vnd.openxmlformats-officedocument.presentationml.presentation sldx=application/vnd.openxmlformats-officedocument.presentationml.slide
    ppsx=application/vnd.openxmlformats-officedocument.presentationml.slideshow potx=application/vnd.openxmlformats-officedocument.presentationml.template xlsx=application/vnd.openxmlformats-officedocument.spreadsheetml.sheet xltx=application/vnd.openxmlformats-officedocument.spreadsheetml.template docx=application/vnd.openxmlformats-officedocument.wordprocessingml.document dotx=application/vnd.openxmlformats-officedocument.wordprocessingml.template mgp=application/vnd.osgeo.mapguide.package dp=application/vnd.osgi.dp
    esa=application/vnd.osgi.subsystem pdb=application/vnd.palm paw=application/vnd.pawaafile str=application/vnd.pg.format ei6=application/vnd.pg.osasli efif=application/vnd.picsel wg=application/vnd.pmi.widget plf=application/vnd.pocketlearn
    pbd=application/vnd.powerbuilder6 box=application/vnd.previewsystems.box mgz=application/vnd.proteus.magazine qps=application/vnd.publishare-delta-tree ptid=application/vnd.pvi.ptid1 qxd=application/vnd.quark.quarkxpress bed=application/vnd.realvnc.bed mxl=application/vnd.recordare.musicxml
    musicxml=application/vnd.recordare.musicxml+xml cryptonote=application/vnd.rig.cryptonote cod=application/vnd.rim.cod rm=application/vnd.rn-realmedia rmvb=application/vnd.rn-realmedia-vbr link66=application/vnd.route66.link66+xml st=application/vnd.sailingtracker.track see=application/vnd.seemail
    sema=application/vnd.sema semd=application/vnd.semd semf=application/vnd.semf ifm=application/vnd.shana.informed.formdata itp=application/vnd.shana.informed.formtemplate iif=application/vnd.shana.informed.interchange ipk=application/vnd.shana.informed.package twd=application/vnd.simtech-mindmapper
    mmf=application/vnd.smaf teacher=application/vnd.smart.teacher sdkm=application/vnd.solent.sdkm+xml dxp=application/vnd.spotfire.dxp sfs=application/vnd.spotfire.sfs sdc=application/vnd.stardivision.calc sda=application/vnd.stardivision.draw sdd=application/vnd.stardivision.impress
    smf=application/vnd.stardivision.math sdw=application/vnd.stardivision.writer sgl=application/vnd.stardivision.writer-global smzip=application/vnd.stepmania.package sm=application/vnd.stepmania.stepchart sxc=application/vnd.sun.xml.calc stc=application/vnd.sun.xml.calc.template sxd=application/vnd.sun.xml.draw
    std=application/vnd.sun.xml.draw.template sxi=application/vnd.sun.xml.impress sti=application/vnd.sun.xml.impress.template sxm=application/vnd.sun.xml.math sxw=application/vnd.sun.xml.writer sxg=application/vnd.sun.xml.writer.global stw=application/vnd.sun.xml.writer.template sus=application/vnd.sus-calendar
    svd=application/vnd.svd sis=application/vnd.symbian.install xsm=application/vnd.syncml+xml bdm=application/vnd.syncml.dm+wbxml xdm=application/vnd.syncml.dm+xml tao=application/vnd.tao.intent-module-archive pcap=application/vnd.tcpdump.pcap tmo=application/vnd.tmobile-livetv
    tpt=application/vnd.trid.tpt mxs=application/vnd.triscape.mxs tra=application/vnd.trueapp ufd=application/vnd.ufdl utz=application/vnd.uiq.theme umj=application/vnd.umajin unityweb=application/vnd.unity uoml=application/vnd.uoml+xml
    vcx=application/vnd.vcx vsd=application/vnd.visio vis=application/vnd.visionary vsf=application/vnd.vsf wbxml=application/vnd.wap.wbxml wmlc=application/vnd.wap.wmlc wmlsc=application/vnd.wap.wmlscriptc wtb=application/vnd.webturbo
    nbp=application/vnd.wolfram.player wpd=application/vnd.wordperfect wqd=application/vnd.wqd stf=application/vnd.wt.stf xar=application/vnd.xara xfdl=application/vnd.xfdl hvd=application/vnd.yamaha.hv-dic hvs=application/vnd.yamaha.hv-script
    hvp=application/vnd.yamaha.hv-voice osf=application/vnd.yamaha.openscoreformat osfpvg=application/vnd.yamaha.openscoreformat.osfpvg+xml saf=application/vnd.yamaha.smaf-audio spf=application/vnd.yamaha.smaf-phrase cmp=application/vnd.yellowriver-custom-menu zir=application/vnd.zul zaz=application/vnd.zzazz.deck+xml
    vxml=application/voicexml+xml wgt=application/widget hlp=application/winhlp wsdl=application/wsdl+xml wspolicy=application/wspolicy+xml 7z=application/x-7z-compressed abw=application/x-abiword ace=application/x-ace-compressed
    dmg=application/x-apple-diskimage aab=application/x-authorware-bin aam=application/x-authorware-map aas=application/x-authorware-seg bcpio=application/x-bcpio torrent=application/x-bittorrent blb=application/x-blorb bz=application/x-bzip
    bz2=application/x-bzip2 cbr=application/x-cbr vcd=application/x-cdlink cfs=application/x-cfs-compressed chat=application/x-chat pgn=application/x-chess-pgn nsc=application/x-conference cpio=application/x-cpio
    csh=application/x-csh deb=application/x-debian-package dgc=application/x-dgc-compressed dir=application/x-director wad=application/x-doom ncx=application/x-dtbncx+xml dtb=application/x-dtbook+xml res=application/x-dtbresource+xml
    dvi=application/x-dvi evy=application/x-envoy eva=application/x-eva bdf=application/x-font-bdf gsf=application/x-font-ghostscript psf=application/x-font-linux-psf otf=application/x-font-otf pcf=application/x-font-pcf
    snf=application/x-font-snf ttf=application/x-font-ttf pfa=application/x-font-type1 woff=application/x-font-woff arc=application/x-freearc spl=application/x-futuresplash gca=application/x-gca-compressed ulx=application/x-glulx
    gnumeric=application/x-gnumeric gramps=application/x-gramps-xml gtar=application/x-gtar hdf=application/x-hdf install=application/x-install-instructions iso=application/x-iso9660-image jnlp=application/x-java-jnlp-file latex=application/x-latex
    lzh=application/x-lzh-compressed mie=application/x-mie prc=application/x-mobipocket-ebook application=application/x-ms-application lnk=application/x-ms-shortcut wmd=application/x-ms-wmd wmz=application/x-ms-wmz xbap=application/x-ms-xbap
    mdb=application/x-msaccess obd=application/x-msbinder crd=application/x-mscardfile clp=application/x-msclip exe=application/x-msdownload mvb=application/x-msmediaview wmf=application/x-msmetafile mny=application/x-msmoney
    pub=application/x-mspublisher scd=application/x-msschedule trm=application/x-msterminal wri=application/x-mswrite nc=application/x-netcdf nzb=application/x-nzb p12=application/x-pkcs12 p7b=application/x-pkcs7-certificates
    p7r=application/x-pkcs7-certreqresp rar=application/x-rar ris=application/x-research-info-systems sh=application/x-sh shar=application/x-shar swf=application/x-shockwave-flash xap=application/x-silverlight-app sql=application/x-sql
    sit=application/x-stuffit sitx=application/x-stuffitx srt=application/x-subrip sv4cpio=application/x-sv4cpio sv4crc=application/x-sv4crc t3=application/x-t3vm-image gam=application/x-tads tar=application/x-tar
    tcl=application/x-tcl tex=application/x-tex tfm=application/x-tex-tfm texinfo=application/x-texinfo obj=application/x-tgif ustar=application/x-ustar src=application/x-wais-source der=application/x-x509-ca-cert
    fig=application/x-xfig xlf=application/x-xliff+xml xpi=application/x-xpinstall xz=application/x-xz z1=application/x-zmachine xaml=application/xaml+xml xdf=application/xcap-diff+xml xenc=application/xenc+xml
    xhtml=application/xhtml+xml xml=application/xml dtd=application/xml-dtd xop=application/xop+xml xpl=application/xproc+xml xslt=application/xslt+xml xspf=application/xspf+xml mxml=application/xv+xml
    yang=application/yang yin=application/yin+xml zip=application/zip adp=audio/adpcm au=audio/basic mid=audio/midi mp3=audio/mpeg mp4a=audio/mp4
    mpga=audio/mpeg oga=audio/ogg s3m=audio/s3m sil=audio/silk uva=audio/vnd.dece.audio eol=audio/vnd.digital-winds dra=audio/vnd.dra dts=audio/vnd.dts
    dtshd=audio/vnd.dts.hd lvp=audio/vnd.lucent.voice pya=audio/vnd.ms-playready.media.pya ecelp4800=audio/vnd.nuera.ecelp4800 ecelp7470=audio/vnd.nuera.ecelp7470 ecelp9600=audio/vnd.nuera.ecelp9600 rip=audio/vnd.rip weba=audio/webm
    aac=audio/x-aac aif=audio/x-aiff caf=audio/x-caf flac=audio/x-flac mka=audio/x-matroska m3u=audio/x-mpegurl wax=audio/x-ms-wax wma=audio/x-ms-wma
    ram=audio/x-pn-realaudio rmp=audio/x-pn-realaudio-plugin wav=audio/x-wav xm=audio/xm cdx=chemical/x-cdx cif=chemical/x-cif cmdf=chemical/x-cmdf cml=chemical/x-cml
    csml=chemical/x-csml xyz=chemical/x-xyz bmp=image/bmp cgm=image/cgm g3=image/g3fax gif=image/gif ief=image/ief jpg=image/jpeg
    jpeg=image/jpeg ktx=image/ktx png=image/png btif=image/prs.btif sgi=image/sgi svg=image/svg+xml tiff=image/tiff psd=image/vnd.adobe.photoshop
    uvi=image/vnd.dece.graphic djvu=image/vnd.djvu dwg=image/vnd.dwg dxf=image/vnd.dxf fbs=image/vnd.fastbidsheet fpx=image/vnd.fpx fst=image/vnd.fst mmr=image/vnd.fujixerox.edmics-mmr
    rlc=image/vnd.fujixerox.edmics-rlc mdi=image/vnd.ms-modi wdp=image/vnd.ms-photo npx=image/vnd.net-fpx wbmp=image/vnd.wap.wbmp xif=image/vnd.xiff webp=image/webp 3ds=image/x-3ds
    ras=image/x-cmu-raster cmx=image/x-cmx fh=image/x-freehand ico=image/x-icon sid=image/x-mrsid-image pcx=image/x-pcx pic=image/x-pict pnm=image/x-portable-anymap
    pbm=image/x-portable-bitmap pgm=image/x-portable-graymap ppm=image/x-portable-pixmap rgb=image/x-rgb tga=image/x-tga xbm=image/x-xbitmap xpm=image/x-xpixmap xwd=image/x-xwindowdump
    eml=message/rfc822 igs=model/iges msh=model/mesh dae=model/vnd.collada+xml dwf=model/vnd.dwf gdl=model/vnd.gdl gtw=model/vnd.gtw mts=model/vnd.mts
    vtu=model/vnd.vtu wrl=model/vrml x3db=model/x3d+binary x3dv=model/x3d+vrml x3d=model/x3d+xml appcache=text/cache-manifest ics=text/calendar css=text/css
    csv=text/csv html=text/html n3=text/n3 txt=text/plain dsc=text/prs.lines.tag rtx=text/richtext rtf=text/rtf sgml=text/sgml
    tsv=text/tab-separated-values t=text/troff ttl=text/turtle uri=text/uri-list vcard=text/vcard curl=text/vnd.curl dcurl=text/vnd.curl.dcurl scurl=text/vnd.curl.scurl
    mcurl=text/vnd.curl.mcurl sub=text/vnd.dvb.subtitle fly=text/vnd.fly flx=text/vnd.fmi.flexstor gv=text/vnd.graphviz 3dml=text/vnd.in3d.3dml spot=text/vnd.in3d.spot jad=text/vnd.sun.j2me.app-descriptor
    wml=text/vnd.wap.wml wmls=text/vnd.wap.wmlscript s=text/x-asm c=text/x-c f=text/x-fortran p=text/x-pascal java=text/x-java-source opml=text/x-opml
    nfo=text/x-nfo etx=text/x-setext sfv=text/x-sfv uu=text/x-uuencode vcs=text/x-vcalendar vcf=text/x-vcard 3gp=video/3gpp 3g2=video/3gpp2
    h261=video/h261 h263=video/h263 h264=video/h264 jpgv=video/jpeg jpm=video/jpm mj2=video/mj2 mp4=video/mp4 mpeg=video/mpeg
    ogv=video/ogg mov=video/quicktime qt=video/quicktime uvh=video/vnd.dece.hd uvm=video/vnd.dece.mobile uvp=video/vnd.dece.pd uvs=video/vnd.dece.sd uvv=video/vnd.dece.video
    dvb=video/vnd.dvb.file fvt=video/vnd.fvt mxu=video/vnd.mpegurl pyv=video/vnd.ms-playready.media.pyv uvu=video/vnd.uvvu.mp4 viv=video/vnd.vivo webm=video/webm f4v=video/x-f4v
    fli=video/x-fli flv=video/x-flv m4v=video/x-m4v mkv=video/x-matroska mng=video/x-mng asf=video/x-ms-asf vob=video/x-ms-vob wm=video/x-ms-wm
    wmv=video/x-ms-wmv wmx=video/x-ms-wmx wvx=video/x-ms-wvx avi=video/x-msvideo movie=video/x-sgi-movie smv=video/x-smv ice=x-conference/x-cooltalk tgz=application/x-gzip
    gz=application/x-gzip";

/// 解析后的只读表：首次查询时构建一次，全程借用。
fn table() -> &'static [(&'static str, &'static str)] {
    static TABLE: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();

    TABLE.get_or_init(|| {
        MIMES_PACKED
            .split_whitespace()
            .filter_map(|pair| pair.split_once('='))
            .collect()
    })
}

/// 由 MIME 类型反查扩展名 —— 对应 PHP 的 `MimeType::search()`。
///
/// `extra` 即配置项 `extra_mime_types`（同为 `扩展名 => 类型`），语义与 PHP 的
/// `array_merge(self::$mimes, $extra)` 一致：**同扩展名由 `extra` 覆盖**（位置不变），
/// `extra` 独有的扩展名排在其后；随后按表序返回第一个值命中的扩展名。
/// 查不到返回 `None`（PHP 返回 `null`，调用方翻译成 `missing_mimetype`）。
pub fn search(mime_type: &str, extra: &[(String, String)]) -> Option<String> {
    for (ext, base_mime) in table() {
        let value = extra
            .iter()
            .find(|(extra_ext, _)| extra_ext == ext)
            .map(|(_, mime)| mime.as_str())
            .unwrap_or(base_mime);

        if value == mime_type {
            return Some((*ext).to_string());
        }
    }

    for (ext, mime) in extra {
        let overrides_base = table().iter().any(|(base_ext, _)| base_ext == ext);
        if !overrides_base && mime == mime_type {
            return Some(ext.clone());
        }
    }

    None
}

/// 由扩展名反查 MIME —— 对应 PHP 的 `MimeType::getMimeTypeFromExtension()`。
///
/// 查不到时返回 `application/octet-stream`（PHP 的默认值）。文件下发时用它填
/// `Content-Type`：与上传时校验用的是**同一张表**，不会出现「校验按 A 表、下发按 B 表」。
pub fn mime_for_extension(extension: &str, extra: &[(String, String)]) -> String {
    let extension = extension.to_ascii_lowercase();

    // extra 与 PHP 的 array_merge 同语义：同扩展名覆盖表内值
    if let Some((_, mime)) = extra.iter().find(|(ext, _)| *ext == extension) {
        return mime.clone();
    }

    for (ext, mime) in table() {
        if *ext == extension {
            return (*mime).to_string();
        }
    }

    "application/octet-stream".to_string()
}

/// 内容类型探测器 —— 对应 PHP 的 `ext-fileinfo`（`mime_content_type()`）。
///
/// PHP 里这是常驻扩展，宿主环境装了就有；Rust 版把它做成可注入的 trait：
/// 默认实现 [`MagicBytesDetector`] 只看魔数（零依赖），需要更精细的判断
/// （如 `.docx` 与 `.zip` 的区分）时，宿主可自行实现 `file(1)` 或
/// `infer` 之类的探测器替换。
pub trait MimeDetector: Send + Sync {
    /// 返回探测到的 MIME 类型；无法判断时返回 `None`。
    fn detect(&self, path: &Path) -> io::Result<Option<String>>;
}

/// 默认探测器：读文件头 512 字节比对魔数，文本回落到 `text/plain`，
/// 其余二进制回落到 `application/octet-stream`。
#[derive(Debug, Default, Clone, Copy)]
pub struct MagicBytesDetector;

impl MimeDetector for MagicBytesDetector {
    fn detect(&self, path: &Path) -> io::Result<Option<String>> {
        let mut file = File::open(path)?;
        let mut head = Vec::new();
        // 只看头部，512 字节足够覆盖这里清单里的全部魔数
        file.by_ref().take(512).read_to_end(&mut head)?;
        Ok(detect_bytes(&head))
    }
}

/// 纯函数：从字节头部判断 MIME（便于单测，不需要真文件）。
///
/// 返回的类型都保证能在 [`table`] 里反查到扩展名，否则内核会在
/// `checkMimeType()` 处报 `missing_mimetype` 而不是给出可用扩展名。
pub fn detect_bytes(head: &[u8]) -> Option<String> {
    let hit = |mime: &str| Some(mime.to_string());

    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return hit("image/png");
    }
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        return hit("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return hit("image/gif");
    }
    if head.starts_with(b"%PDF") {
        return hit("application/pdf");
    }
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return hit("application/zip");
    }
    if head.starts_with(b"Rar!\x1a\x07") {
        return hit("application/x-rar");
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return hit("application/x-gzip");
    }
    if head.starts_with(&[0x7f, b'E', b'L', b'F']) || head.starts_with(b"MZ") {
        return hit("application/x-msdownload");
    }
    if head.starts_with(b"OggS") {
        return hit("audio/ogg");
    }
    if head.starts_with(b"fLaC") {
        return hit("audio/x-flac");
    }
    if head.starts_with(b"ID3") || (head.len() >= 2 && head[0] == 0xff && head[1] & 0xe0 == 0xe0) {
        return hit("audio/mpeg");
    }
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return hit("video/webm");
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" {
        return match &head[8..12] {
            b"WEBP" => hit("image/webp"),
            b"WAVE" => hit("audio/x-wav"),
            b"AVI " => hit("video/x-msvideo"),
            _ => hit("application/octet-stream"),
        };
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        // ftyp 品牌：qt 系列走 quicktime，其余按 mp4 处理
        return match &head[8..12] {
            b"qt  " => hit("video/quicktime"),
            _ => hit("video/mp4"),
        };
    }
    if head.starts_with(b"BM") && head.len() >= 14 && head[6..14] != [0; 8] {
        return hit("image/bmp");
    }

    // 文本：SVG 先判，再回落纯文本（PHP 的 fileinfo 同样把文本判成 text/plain）。
    // 含 NUL 的一律当二进制 —— UTF-8 里 0x00 是合法字符，不排掉会把「一堆控制字节」
    // 判成 text/plain（file(1) 也以 NUL 作为二进制判据）。
    if !head.contains(&0)
        && let Ok(text) = std::str::from_utf8(head)
    {
        let trimmed = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
        if trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg"))
        {
            return hit("image/svg+xml");
        }
        return hit("text/plain");
    }

    hit("application/octet-stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_complete() {
        // 769 = 770 - 1（PHP 数组里重复的 tar 键去重后）
        assert_eq!(table().len(), 769);
        assert!(
            table()
                .iter()
                .all(|(ext, mime)| !ext.is_empty() && mime.contains('/'))
        );
    }

    #[test]
    fn search_matches_php_semantics() {
        assert_eq!(search("image/png", &[]).as_deref(), Some("png"));
        assert_eq!(search("application/pdf", &[]).as_deref(), Some("pdf"));
        // 同名 MIME 取表中第一个命中的扩展名
        assert_eq!(search("application/zip", &[]).as_deref(), Some("zip"));
        assert_eq!(search("no/such-type", &[]), None);

        // extra 覆盖：自定义扩展名把 jpg 映射改掉后，image/jpeg 反查得到 jpeg（表里第二个）
        let extra = vec![("jpg".to_string(), "image/custom".to_string())];
        assert_eq!(search("image/custom", &extra).as_deref(), Some("jpg"));
        assert_eq!(search("image/jpeg", &extra).as_deref(), Some("jpeg"));

        // extra 独有扩展名排在表后
        let extra = vec![("foo".to_string(), "application/x-foo".to_string())];
        assert_eq!(search("application/x-foo", &extra).as_deref(), Some("foo"));
    }

    #[test]
    fn magic_bytes_cover_common_uploads() {
        assert_eq!(
            detect_bytes(b"\x89PNG\r\n\x1a\n....").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            detect_bytes(b"\xff\xd8\xff\xe0..").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(detect_bytes(b"GIF89a...").as_deref(), Some("image/gif"));
        assert_eq!(
            detect_bytes(b"%PDF-1.7").as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            detect_bytes(b"PK\x03\x04....").as_deref(),
            Some("application/zip")
        );
        assert_eq!(
            detect_bytes(b"RIFF\x00\x00\x00\x00WEBPVP8 ").as_deref(),
            Some("image/webp")
        );
        assert_eq!(
            detect_bytes(b"\x00\x00\x00\x18ftypmp42").as_deref(),
            Some("video/mp4")
        );
        assert_eq!(
            detect_bytes(b"<svg xmlns=\"...\">").as_deref(),
            Some("image/svg+xml")
        );
        assert_eq!(
            detect_bytes("Hello, 世界".as_bytes()).as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            detect_bytes(&[0x00, 0x01, 0x02, 0x03]).as_deref(),
            Some("application/octet-stream")
        );
    }

    /// 探测出的每个类型都必须能在表里反查到扩展名，否则末块校验必然
    /// 卡在 `missing_mimetype` —— 这是探测器与表之间的对账。
    #[test]
    fn every_detected_mime_resolves_to_an_extension() {
        let samples: &[&[u8]] = &[
            b"\x89PNG\r\n\x1a\n",
            b"\xff\xd8\xff",
            b"GIF89a",
            b"%PDF",
            b"PK\x03\x04",
            b"Rar!\x1a\x07",
            b"\x1f\x8b",
            b"MZ",
            b"OggS",
            b"fLaC",
            b"ID3",
            b"\x1a\x45\xdf\xa3",
            b"RIFF\x00\x00\x00\x00WEBP",
            b"RIFF\x00\x00\x00\x00WAVE",
            b"RIFF\x00\x00\x00\x00AVI ",
            b"\x00\x00\x00\x18ftypmp42",
            b"\x00\x00\x00\x18ftypqt  ",
            b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00",
            b"<svg></svg>",
            b"plain text",
            &[0x00, 0x01],
        ];

        for sample in samples {
            let mime = detect_bytes(sample).expect("探测器必须给出兜底类型");
            assert!(
                search(&mime, &[]).is_some(),
                "{mime} 在 MIME 表里没有对应扩展名"
            );
        }
    }
}
