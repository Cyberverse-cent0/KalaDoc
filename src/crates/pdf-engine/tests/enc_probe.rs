use kaladoc::crates::pdf_engine::{PdfEngine, PdfPermissions};
use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};

fn pdf_one() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let content = Content { operations: vec![Operation::new("BT", vec![]), Operation::new("ET", vec![])] };
    let cid = doc.add_object(Stream::new(dictionary!{}, content.encode().unwrap()));
    let pid = doc.add_object(dictionary!{"Type"=>"Page","Parent"=>pages_id,"Contents"=>cid,"MediaBox"=>vec![0.into(),0.into(),595.into(),842.into()]});
    doc.objects.insert(pages_id, Object::Dictionary(dictionary!{"Type"=>"Pages","Kids"=>vec![Object::Reference(pid)],"Count"=>1}));
    let cat = doc.add_object(dictionary!{"Type"=>"Catalog","Pages"=>pages_id});
    doc.trailer.set("Root", cat);
    let mut b=Vec::new(); doc.save_to(&mut b).unwrap(); b
}

#[test]
fn probe() {
    let eng = PdfEngine::new();
    let enc = eng.encrypt(&pdf_one(), "pw", None, PdfPermissions::default()).unwrap();
    let doc = Document::load_mem(&enc).unwrap();
    eprintln!("objects={} pages={} encrypted={} trailerEncrypt={}", doc.objects.len(), doc.get_pages().len(), doc.is_encrypted(), doc.trailer.has(b"Encrypt"));
    let audit = eng.audit_content(&enc).unwrap();
    eprintln!("audit pages={} warnings={:?} risks={}", audit.pages_scanned, audit.warnings, audit.risks.len());
}
