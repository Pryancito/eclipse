use super::*;

fn page(p: &Panel) -> Layout {
    layout(1600, 900, &p.cat, &p.shown(), p.top, p.lang)
}

fn panel() -> Panel {
    let cat = Catalogo::de_serie();
    let actual = cat.claves.iter().map(|c| c.normaliza(None)).collect();
    Panel::new(cat, actual, Lang::Es)
}

#[test]
fn every_measure_the_computation_makes_is_on_the_grid() {
    let mut p = panel();
    for g in 0..p.cat.grupos.len() {
        p.pagina(g);
        let l = page(&p);
        assert_eq!(l.label_w % 4, 0);
        assert_eq!(l.nav_w % 4, 0);
        for f in &l.filas {
            let widths: Vec<i32> = match &f.control {
                Control::Fichas(r) => r.iter().map(|r| r.2).collect(),
                Control::Lista { caja, .. } => vec![caja.2],
                Control::Interruptor(r) | Control::Boton(r) => vec![r.2],
                Control::Medidor { segs, caja } => {
                    segs.iter().map(|r| r.2).chain([caja.2]).collect()
                }
            };
            for w in widths {
                assert_eq!(w % 4, 0, "{} has a width of {w}", p.cat.claves[f.clave].id);
            }
        }
    }
}

#[test]
fn the_control_follows_from_the_data() {
    let p = panel();
    let c = |id: &str| p.cat.claves.iter().find(|c| c.id == id).unwrap();
    assert!(
        matches!(control(c("aspecto"), 0, 0, 800, Lang::Es), Control::Fichas(ref r) if r.len() == 5)
    );
    // The same five values in too little room become a stepper.
    assert!(matches!(
        control(c("aspecto"), 0, 0, 200, Lang::Es),
        Control::Lista { .. }
    ));
    assert!(matches!(
        control(c("teclado.idioma"), 0, 0, 800, Lang::Es),
        Control::Interruptor(_)
    ));
    assert!(matches!(
        control(c("red"), 0, 0, 800, Lang::Es),
        Control::Boton(_)
    ));

    let mut cat = Catalogo::default();
    cat.lee_texto(
        "[n]\ngrupo = g\ntipo = numero\nfichero = /x\nclave = k\nrango = 0..10\n\
         [m]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = k\nvalores = a b c d e f g\n",
        "t",
    );
    match control(&cat.claves[0], 0, 0, 800, Lang::Es) {
        Control::Medidor { segs, .. } => assert_eq!(segs.len(), 11),
        c => panic!("{c:?}"),
    }
    // Seven values are past what a row of chips should hold.
    assert!(matches!(
        control(&cat.claves[1], 0, 0, 2000, Lang::Es),
        Control::Lista { .. }
    ));
}

#[test]
fn the_label_column_is_the_longest_label_on_the_page() {
    let mut p = panel();
    let g = p.cat.grupos.iter().position(|g| g.id == "idioma").unwrap();
    p.pagina(g);
    let l = page(&p);
    let longest = p
        .shown()
        .iter()
        .map(|&i| {
            let c = &p.cat.claves[i];
            Canvas::text_width(c.nombre(false)).max(Canvas::text_width(&c.origen()))
        })
        .max()
        .unwrap();
    assert!(
        l.label_w >= longest && l.label_w < longest + 2 * PAD + 4,
        "{} vs {longest}",
        l.label_w
    );
    // Controls start after it and stay inside the panel.
    for f in &l.filas {
        let b = control_box(&f.control);
        assert!(b.0 >= f.rect.0 + l.label_w);
        assert!(b.0 + b.2 <= l.panel.0 + l.panel.2, "{:?}", f.control);
    }
}

#[test]
fn a_held_change_is_dropped_by_undo_and_by_choosing_the_old_value() {
    let mut p = panel();
    let i = p.cat.claves.iter().position(|c| c.id == "aspecto").unwrap();
    p.elige(i, "kde".into());
    assert_eq!(p.pendientes(), 1);
    assert_eq!(p.valor(i), "kde");
    p.elige(i, "tecno".into());
    assert_eq!(p.pendientes(), 0);
    p.elige(i, "gnome".into());
    p.elige(i, "nope".into());
    assert_eq!(p.valor(i), "gnome");
    p.deshaz();
    assert_eq!(p.pendientes(), 0);
    assert_eq!(p.valor(i), "tecno");
}

#[test]
fn typing_searches_across_every_page() {
    let mut p = panel();
    p.busca = "teclado".into();
    let ids: Vec<&str> = p
        .shown()
        .iter()
        .map(|&i| p.cat.claves[i].id.as_str())
        .collect();
    assert_eq!(ids, ["teclado", "teclado.idioma"]);
    p.mueve(5, 3);
    assert_eq!(p.foco, 1);
}

#[test]
fn every_page_and_size_draws_without_a_panic() {
    for lang in [Lang::Es, Lang::En] {
        let mut p = panel();
        p.lang = lang;
        for g in 0..p.cat.grupos.len() {
            p.pagina(g);
            for (w, h) in [(1600, 900), (1024, 600), (640, 480), (320, 240)] {
                draw(&mut Canvas::new(w, h), w as i32, h as i32, &p);
            }
        }
        p.busca = "zzz".into();
        draw(&mut Canvas::new(800, 600), 800, 600, &p);
    }
    let mut empty = Panel::new(Catalogo::default(), Vec::new(), Lang::Es);
    empty.status = "x".into();
    draw(&mut Canvas::new(800, 600), 800, 600, &empty);
}

#[test]
fn the_services_and_openers_pages_draw_their_rows_at_every_size() {
    for lang in [Lang::Es, Lang::En] {
        let mut p = panel();
        p.lang = lang;
        tablas::muestra(&mut p);
        for id in ["servicios", "aperturas"] {
            p.grupo = group_index(&p.cat, Some(id));
            assert!(p.pagina_serv().is_some() || p.pagina_aper().is_some(), "{id}");
            for (w, h) in [(1600, 900), (1024, 600), (640, 480), (320, 240)] {
                draw(&mut Canvas::new(w, h), w as i32, h as i32, &p);
            }
        }
        assert_eq!(p.cuenta_grupo(group_index(&p.cat, Some("servicios"))), 13);
    }
}

#[test]
fn a_held_boot_or_program_is_dropped_by_choosing_what_is_in_force() {
    let mut p = panel();
    tablas::muestra(&mut p);
    p.serv.pendiente.clear();
    p.serv.elige("hunterd", false);
    assert_eq!(p.serv.pendiente.get("hunterd"), Some(&false));
    p.serv.elige("hunterd", true);
    assert!(p.serv.pendiente.is_empty());
    // An essential service is never held off.
    p.serv.elige("labwc", false);
    assert!(p.serv.pendiente.is_empty());

    p.aper.pendiente.clear();
    p.aper.paso("txt", 1);
    assert_eq!(
        p.aper.pendiente.get("txt"),
        Some(&Some("eclipse-view".to_string()))
    );
    p.aper.paso("txt", -1);
    assert!(p.aper.pendiente.is_empty());
    // `.svg` is the viewer in the file: "as the image does" is held.
    p.aper.elige("svg", None);
    assert_eq!(p.aper.pendiente.get("svg"), Some(&None));
    // The driver installer is not one of the page's programs.
    p.aper.paso("drv", 1);
    assert!(!p.aper.pendiente.contains_key("drv"));
}
