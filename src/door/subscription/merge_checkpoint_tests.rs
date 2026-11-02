use super::*;

#[test]
fn merges_preserve_certified_prefix_at_frozen_target_across_newer_proof() {
    for shape in ["left", "right", "bridge", "unchecked-left"] {
        let f = Fixture::new();
        let s = f.open();
        let sub = sub(&f, &s);
        let tx = begin(&f);
        selection::exclude(&tx, &sub, &span(10, 200)).unwrap();
        if shape == "bridge" {
            selection::exclude(&tx, &sub, &span(202, 400)).unwrap();
        }
        selection::advance_revision(&tx).unwrap();
        selection::start_revisit(&tx, &sub).unwrap();
        let work = selection::revisit(&tx, &sub).unwrap().unwrap();
        selection::checkpoint(&tx, &sub, &work, 128).unwrap();
        // New traffic may have even newer proof than the pending target.
        selection::advance_revision(&tx).unwrap();
        let mut added = match shape {
            "left" | "unchecked-left" => span(1, 9),
            "right" => span(201, 210),
            "bridge" => span(201, 201),
            _ => unreachable!(),
        };
        if shape != "unchecked-left" {
            added.checked = 2;
            added.scan = 2;
        }
        selection::exclude(&tx, &sub, &added).unwrap();
        let work = selection::revisit(&tx, &sub).unwrap().unwrap();
        assert_eq!(work.scan, 1, "{shape}: preserve in-flight target");
        assert_eq!(
            work.through,
            if shape == "unchecked-left" { 0 } else { 128 },
            "{shape}"
        );
        selection::checkpoint(&tx, &sub, &work, work.last).unwrap();
        assert!(selection::revisit(&tx, &sub).unwrap().is_none());
        selection::finish_revisit(&tx, &sub).unwrap();
        assert_eq!(selection::start_revisit(&tx, &sub).unwrap(), 2);
        let next = selection::revisit(&tx, &sub).unwrap().unwrap();
        assert_eq!(
            next.through,
            next.first - 1,
            "new target rechecks whole span"
        );
        tx.commit().unwrap();
    }
}
